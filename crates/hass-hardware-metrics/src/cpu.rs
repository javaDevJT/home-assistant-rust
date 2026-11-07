#[cfg(any(target_os = "linux", test))]
use crate::core_error;
use crate::round_one_decimal;

/// Aggregate counters in the same order-neutral form used by psutil.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CpuCounters {
    pub(super) user: u64,
    pub(super) nice: u64,
    pub(super) system: u64,
    pub(super) idle: u64,
    pub(super) iowait: u64,
    pub(super) irq: u64,
    pub(super) softirq: u64,
    pub(super) steal: u64,
    pub(super) guest: u64,
    pub(super) guest_nice: u64,
}

/// Sample a psutil-style non-blocking CPU percentage using an injected reader.
///
/// An absent baseline causes two consecutive reads; later calls use the
/// retained baseline and make exactly one read. State only advances after a
/// successful sample, matching psutil's behavior when a counter read fails.
pub(super) fn sample_cpu_percent(
    baseline: &mut Option<CpuCounters>,
    mut read: impl FnMut() -> hass_core::Result<CpuCounters>,
) -> hass_core::Result<f64> {
    let (previous, current) = match *baseline {
        Some(previous) => (previous, read()?),
        None => {
            let first = read()?;
            (first, read()?)
        }
    };

    *baseline = Some(current);
    Ok(cpu_percent(previous, current))
}

/// Calculate psutil's aggregate CPU percentage from two counter snapshots.
pub(super) fn cpu_percent(previous: CpuCounters, current: CpuCounters) -> f64 {
    let deltas = CpuCounters {
        user: delta(previous.user, current.user),
        nice: delta(previous.nice, current.nice),
        system: delta(previous.system, current.system),
        idle: delta(previous.idle, current.idle),
        iowait: delta(previous.iowait, current.iowait),
        irq: delta(previous.irq, current.irq),
        softirq: delta(previous.softirq, current.softirq),
        steal: delta(previous.steal, current.steal),
        guest: delta(previous.guest, current.guest),
        guest_nice: delta(previous.guest_nice, current.guest_nice),
    };

    // Linux reports guest time both inside user/nice and in separate fields.
    let total = deltas.user as i128
        + deltas.nice as i128
        + deltas.system as i128
        + deltas.idle as i128
        + deltas.iowait as i128
        + deltas.irq as i128
        + deltas.softirq as i128
        + deltas.steal as i128
        + deltas.guest as i128
        + deltas.guest_nice as i128
        - deltas.guest as i128
        - deltas.guest_nice as i128;
    if total == 0 {
        return 0.0;
    }

    let busy = total - deltas.idle as i128 - deltas.iowait as i128;
    round_one_decimal((busy as f64 / total as f64) * 100.0)
}

fn delta(previous: u64, current: u64) -> u64 {
    current.saturating_sub(previous)
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn malformed_cpu_sample(detail: impl std::fmt::Display) -> hass_core::Error {
    core_error(format!("invalid CPU counter sample: {detail}"))
}
