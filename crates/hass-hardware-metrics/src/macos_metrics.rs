use crate::cpu::CpuCounters;
use crate::memory::{MemorySample, memory_sample};

/// Mach `host_cpu_load_info_data_t` counters are user, system, idle, nice.
pub(super) fn cpu_counters_from_mach(ticks: [u32; 4]) -> CpuCounters {
    CpuCounters {
        user: u64::from(ticks[0]),
        system: u64::from(ticks[1]),
        idle: u64::from(ticks[2]),
        nice: u64::from(ticks[3]),
        ..CpuCounters::default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct VmPageCounts {
    pub(super) active: u64,
    pub(super) inactive: u64,
    pub(super) free: u64,
    pub(super) speculative: u64,
}

/// Convert `host_statistics64` page counts using psutil's macOS semantics.
pub(super) fn memory_from_vm_pages(
    total_bytes: u64,
    page_size: u64,
    pages: VmPageCounts,
) -> hass_core::Result<MemorySample> {
    if page_size == 0 {
        return Err(crate::core_error("macOS page size is zero"));
    }
    // psutil's available is inactive + free. `active` and `speculative` only
    // contribute to its separate used/free fields, which HA does not report.
    let available_pages = pages
        .inactive
        .checked_add(pages.free)
        .ok_or_else(|| crate::core_error("macOS available-page count overflows"))?;
    let available_bytes = available_pages
        .checked_mul(page_size)
        .ok_or_else(|| crate::core_error("macOS available-memory byte count overflows"))?;
    memory_sample(total_bytes, available_bytes)
}
