use std::mem::MaybeUninit;

use crate::core_error;
use crate::cpu::CpuCounters;
use crate::macos_metrics::{VmPageCounts, cpu_counters_from_mach, memory_from_vm_pages};
use crate::memory::MemorySample;

pub(super) fn read_cpu_counters() -> hass_core::Result<CpuCounters> {
    // SAFETY: mach_host_self is the libc binding for the read-only host port.
    #[allow(deprecated)]
    let host = unsafe { libc::mach_host_self() };
    if host == 0 {
        return Err(core_error("mach_host_self returned MACH_PORT_NULL"));
    }

    let mut info = MaybeUninit::<libc::host_cpu_load_info_data_t>::zeroed();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `info` is large enough for host_cpu_load_info_data_t and `count`
    // is the matching Mach word count. The function only reads host counters.
    let status = unsafe {
        libc::host_statistics(
            host,
            libc::HOST_CPU_LOAD_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    // SAFETY: both calls use the current task/host ports and release the
    // send-right returned by mach_host_self, following psutil's C collector.
    unsafe {
        let task = mach2::traps::mach_task_self();
        let _ = mach2::mach_port::mach_port_deallocate(task, host);
    }

    if status != libc::KERN_SUCCESS {
        return Err(core_error(format!(
            "host_statistics(HOST_CPU_LOAD_INFO) failed with Mach status {status}"
        )));
    }
    if count != libc::HOST_CPU_LOAD_INFO_COUNT {
        return Err(core_error(format!(
            "host_statistics(HOST_CPU_LOAD_INFO) returned {count} words, expected {}",
            libc::HOST_CPU_LOAD_INFO_COUNT
        )));
    }

    // SAFETY: successful host_statistics filled the complete advertised result.
    let info = unsafe { info.assume_init() };
    Ok(cpu_counters_from_mach(info.cpu_ticks))
}

pub(super) fn read_memory_sample() -> hass_core::Result<MemorySample> {
    let total_bytes = read_total_memory()?;
    let pages = read_vm_page_counts()?;
    // SAFETY: sysconf is a read-only libc query for the current page size.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size)
        .ok()
        .filter(|page_size| *page_size > 0)
        .ok_or_else(|| core_error("getpagesize returned an invalid value"))?;

    memory_from_vm_pages(total_bytes, page_size, pages)
}

fn read_total_memory() -> hass_core::Result<u64> {
    let mut total_bytes = 0_u64;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: the name is NUL-terminated and the output pointer/length match
    // the u64 `hw.memsize` result documented by psutil's sysctl collector.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total_bytes as *mut u64).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(core_error(format!(
            "sysctlbyname(hw.memsize) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if length != std::mem::size_of::<u64>() {
        return Err(core_error(format!(
            "sysctlbyname(hw.memsize) returned {length} bytes, expected {}",
            std::mem::size_of::<u64>()
        )));
    }
    Ok(total_bytes)
}

fn read_vm_page_counts() -> hass_core::Result<VmPageCounts> {
    // SAFETY: mach_host_self returns a read-only host port used by the Mach
    // statistics call below.
    #[allow(deprecated)]
    let host = unsafe { libc::mach_host_self() };
    if host == 0 {
        return Err(core_error("mach_host_self returned MACH_PORT_NULL"));
    }

    let mut info = MaybeUninit::<libc::vm_statistics64_data_t>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `info` is the libc vm_statistics64 type and count is derived
    // from that exact type. The call writes only the advertised result.
    let status = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    // SAFETY: releases the host send-right just acquired above.
    unsafe {
        let task = mach2::traps::mach_task_self();
        let _ = mach2::mach_port::mach_port_deallocate(task, host);
    }

    if status != libc::KERN_SUCCESS {
        return Err(core_error(format!(
            "host_statistics64(HOST_VM_INFO64) failed with Mach status {status}"
        )));
    }
    if count < 3 {
        return Err(core_error(format!(
            "host_statistics64(HOST_VM_INFO64) returned only {count} words"
        )));
    }

    // SAFETY: successful host_statistics64 filled the advertised structure.
    let info = unsafe { info.assume_init() };
    Ok(VmPageCounts {
        active: u64::from(info.active_count),
        inactive: u64::from(info.inactive_count),
        free: u64::from(info.free_count),
        speculative: u64::from(info.speculative_count),
    })
}
