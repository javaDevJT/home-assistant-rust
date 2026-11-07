use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;

use crate::core_error;
use crate::cpu::{CpuCounters, malformed_cpu_sample};
use crate::memory::{MemorySample, memory_sample};

const MEMINFO_PATH: &str = "/proc/meminfo";
const ZONEINFO_PATH: &str = "/proc/zoneinfo";

#[cfg(target_os = "linux")]
pub(super) fn read_cpu_counters() -> hass_core::Result<CpuCounters> {
    let contents = fs::read_to_string("/proc/stat")
        .map_err(|error| core_error(format!("cannot read /proc/stat: {error}")))?;
    parse_cpu_stat(&contents)
}

#[cfg(target_os = "linux")]
pub(super) fn read_memory_sample() -> hass_core::Result<MemorySample> {
    let contents = fs::read_to_string(MEMINFO_PATH)
        .map_err(|error| core_error(format!("cannot read {MEMINFO_PATH}: {error}")))?;
    let meminfo = parse_meminfo(&contents)?;

    let needs_fallback = meminfo
        .get("MemAvailable:")
        .is_none_or(|available| *available == 0);
    let has_zone_inputs = ["Active(file):", "Inactive(file):", "SReclaimable:"]
        .iter()
        .all(|key| meminfo.contains_key(*key));
    let zoneinfo = if needs_fallback && has_zone_inputs {
        // psutil falls back to MemFree + Cached if /proc/zoneinfo is unavailable.
        fs::read_to_string(ZONEINFO_PATH).ok()
    } else {
        None
    };

    let page_size = if needs_fallback && has_zone_inputs && zoneinfo.is_some() {
        linux_page_size()?
    } else {
        // This value is unused by the MemAvailable and MemFree + Cached paths.
        1
    };
    memory_from_meminfo(&meminfo, zoneinfo.as_deref(), page_size)
}

/// Parse the aggregate `/proc/stat` CPU row without reading the current host.
pub(super) fn parse_cpu_stat(contents: &str) -> hass_core::Result<CpuCounters> {
    let line = contents
        .lines()
        .next()
        .ok_or_else(|| malformed_cpu_sample("/proc/stat is empty"))?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("cpu") {
        return Err(malformed_cpu_sample("aggregate row must begin with `cpu`"));
    }

    let mut counters = [0_u64; 10];
    let mut count = 0;
    for field in fields.take(counters.len()) {
        counters[count] = field
            .parse::<u64>()
            .map_err(|error| malformed_cpu_sample(format!("invalid counter `{field}`: {error}")))?;
        count += 1;
    }
    if count < 4 {
        return Err(malformed_cpu_sample(format!(
            "aggregate row has {count} counters; at least four are required"
        )));
    }

    Ok(CpuCounters {
        user: counters[0],
        nice: counters[1],
        system: counters[2],
        idle: counters[3],
        iowait: counters[4],
        irq: counters[5],
        softirq: counters[6],
        steal: counters[7],
        guest: counters[8],
        guest_nice: counters[9],
    })
}

/// Parse `/proc/meminfo` values into bytes, retaining signed values for the
/// source algorithm's negative-available clamp.
pub(super) fn parse_meminfo(contents: &str) -> hass_core::Result<HashMap<String, i128>> {
    let mut meminfo = HashMap::new();
    for (line_number, line) in contents.lines().enumerate() {
        let mut fields = line.split_whitespace();
        let key = fields.next().ok_or_else(|| {
            core_error(format!(
                "{MEMINFO_PATH}: malformed empty line {}",
                line_number + 1
            ))
        })?;
        let raw = fields.next().ok_or_else(|| {
            core_error(format!(
                "{MEMINFO_PATH}: missing value for {key} on line {}",
                line_number + 1
            ))
        })?;
        let kibibytes = raw.parse::<i128>().map_err(|error| {
            core_error(format!(
                "{MEMINFO_PATH}: invalid value `{raw}` for {key} on line {}: {error}",
                line_number + 1
            ))
        })?;
        let bytes = kibibytes
            .checked_mul(1024)
            .ok_or_else(|| core_error(format!("{MEMINFO_PATH}: byte count overflows for {key}")))?;
        // The source parser overwrites a repeated key with its last occurrence.
        meminfo.insert(key.to_owned(), bytes);
    }
    Ok(meminfo)
}

/// Reproduce psutil's Linux MemAvailable and pre-3.14 zone-watermark fallback.
pub(super) fn memory_from_meminfo(
    meminfo: &HashMap<String, i128>,
    zoneinfo: Option<&str>,
    page_size: u64,
) -> hass_core::Result<MemorySample> {
    let total = required(meminfo, "MemTotal:")?;
    let free = required(meminfo, "MemFree:")?;
    let mut available = match meminfo.get("MemAvailable:") {
        Some(available) if *available != 0 => *available,
        _ => calculate_avail_vmem(meminfo, zoneinfo, page_size)?,
    };

    // psutil clamps negative available to zero and replaces an over-total
    // estimate with MemFree; it does not silently clamp to MemTotal.
    if available < 0 {
        available = 0;
    } else if available > total {
        available = free;
    }

    let total_bytes = u64::try_from(total)
        .map_err(|_| core_error(format!("invalid MemTotal value: {total} bytes")))?;
    let available_bytes = u64::try_from(available).map_err(|_| {
        core_error(format!(
            "invalid MemAvailable value after psutil clamp: {available} bytes"
        ))
    })?;
    memory_sample(total_bytes, available_bytes)
}

fn calculate_avail_vmem(
    meminfo: &HashMap<String, i128>,
    zoneinfo: Option<&str>,
    page_size: u64,
) -> hass_core::Result<i128> {
    let free = required(meminfo, "MemFree:")?;
    let cached = meminfo.get("Cached:").copied().unwrap_or(0);
    let fallback = free
        .checked_add(cached)
        .ok_or_else(|| core_error("MemFree + Cached overflows"))?;

    let (Some(active_file), Some(inactive_file), Some(slab_reclaimable)) = (
        meminfo.get("Active(file):").copied(),
        meminfo.get("Inactive(file):").copied(),
        meminfo.get("SReclaimable:").copied(),
    ) else {
        return Ok(fallback);
    };
    let Some(zoneinfo) = zoneinfo else {
        return Ok(fallback);
    };
    if page_size == 0 {
        return Err(core_error("Linux page size is zero"));
    }

    let low_pages = sum_zone_low_watermarks(zoneinfo)?;
    let watermark_low = low_pages
        .checked_mul(page_size as i128)
        .ok_or_else(|| core_error("Linux low watermarks overflow byte count"))?;
    let page_cache = active_file
        .checked_add(inactive_file)
        .ok_or_else(|| core_error("file page-cache counters overflow"))?;
    let twice_watermark = watermark_low
        .checked_mul(2)
        .ok_or_else(|| core_error("Linux low watermark overflows"))?;

    // psutil uses Python `/` here, then truncates with int() at return. Keep
    // half-byte intermediates exact by doing the same expression doubled.
    let page_cache_adjustment_twice = page_cache.min(twice_watermark);
    let slab_adjustment_twice = slab_reclaimable.min(twice_watermark);
    let available_twice = free
        .checked_mul(2)
        .and_then(|value| value.checked_sub(twice_watermark))
        .and_then(|value| value.checked_add(page_cache.checked_mul(2)?))
        .and_then(|value| value.checked_sub(page_cache_adjustment_twice))
        .and_then(|value| value.checked_add(slab_reclaimable.checked_mul(2)?))
        .and_then(|value| value.checked_sub(slab_adjustment_twice))
        .ok_or_else(|| core_error("Linux available-memory fallback overflows"))?;
    Ok(available_twice / 2)
}

fn sum_zone_low_watermarks(zoneinfo: &str) -> hass_core::Result<i128> {
    let mut total = 0_i128;
    for (line_number, line) in zoneinfo.lines().enumerate() {
        let line = line.trim();
        // This deliberately matches psutil's startswith(b"low") behavior.
        if !line.starts_with("low") {
            continue;
        }
        let value = line.split_whitespace().nth(1).ok_or_else(|| {
            core_error(format!(
                "{ZONEINFO_PATH}: missing low watermark on line {}",
                line_number + 1
            ))
        })?;
        let pages = value.parse::<i128>().map_err(|error| {
            core_error(format!(
                "{ZONEINFO_PATH}: invalid low watermark `{value}` on line {}: {error}",
                line_number + 1
            ))
        })?;
        total = total
            .checked_add(pages)
            .ok_or_else(|| core_error("Linux low watermarks overflow"))?;
    }
    Ok(total)
}

fn required(meminfo: &HashMap<String, i128>, key: &str) -> hass_core::Result<i128> {
    meminfo
        .get(key)
        .copied()
        .ok_or_else(|| core_error(format!("{MEMINFO_PATH}: required field {key} is missing")))
}

#[cfg(target_os = "linux")]
fn linux_page_size() -> hass_core::Result<u64> {
    // SAFETY: sysconf is a read-only libc query for the current page size.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page_size)
        .ok()
        .filter(|page_size| *page_size > 0)
        .ok_or_else(|| core_error("libc::sysconf(_SC_PAGESIZE) returned an invalid value"))
}
