use crate::{core_error, round_one_decimal};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MemorySample {
    pub(super) total_bytes: u64,
    pub(super) available_bytes: u64,
    pub(super) used_percent: f64,
}

pub(super) fn memory_sample(
    total_bytes: u64,
    available_bytes: u64,
) -> hass_core::Result<MemorySample> {
    if total_bytes == 0 {
        return Err(core_error("total physical memory is zero"));
    }
    if available_bytes > total_bytes {
        return Err(core_error(format!(
            "available memory ({available_bytes} bytes) exceeds total memory ({total_bytes} bytes)"
        )));
    }

    let used_bytes = total_bytes - available_bytes;
    let used_percent = round_one_decimal(used_bytes as f64 / total_bytes as f64 * 100.0);
    Ok(MemorySample {
        total_bytes,
        available_bytes,
        used_percent,
    })
}
