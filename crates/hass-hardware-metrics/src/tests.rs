use std::collections::HashMap;

use crate::cpu::{CpuCounters, cpu_percent, sample_cpu_percent};
use crate::linux::{memory_from_meminfo, parse_cpu_stat, parse_meminfo};
use crate::macos_metrics::{VmPageCounts, cpu_counters_from_mach, memory_from_vm_pages};

fn cpu(
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    guest: u64,
    guest_nice: u64,
) -> CpuCounters {
    CpuCounters {
        user,
        nice,
        system,
        idle,
        iowait,
        guest,
        guest_nice,
        ..CpuCounters::default()
    }
}

#[test]
fn first_cpu_sample_uses_two_reads_and_later_sample_retains_baseline() {
    let samples = [cpu(100, 0, 20, 80, 0, 0, 0), cpu(101, 0, 20, 80, 0, 0, 0)];
    let mut baseline = None;
    let mut first_index = 0;
    let first = sample_cpu_percent(&mut baseline, || {
        let sample = samples[first_index];
        first_index += 1;
        Ok(sample)
    })
    .expect("two injected first samples");
    assert_eq!(first_index, 2);
    assert_eq!(first, 100.0);

    let later = sample_cpu_percent(&mut baseline, || {
        first_index += 1;
        Ok(cpu(102, 0, 20, 80, 0, 0, 0))
    })
    .expect("one later sample");
    assert_eq!(first_index, 3);
    assert_eq!(later, 100.0);
    assert_eq!(baseline, Some(cpu(102, 0, 20, 80, 0, 0, 0)));
}

#[test]
fn cpu_read_error_keeps_the_previous_baseline() {
    let mut baseline = Some(cpu(10, 0, 0, 10, 0, 0, 0));
    let error = sample_cpu_percent(&mut baseline, || {
        Err(crate::core_error("fixture CPU read failed"))
    });
    assert!(error.is_err());
    assert_eq!(baseline, Some(cpu(10, 0, 0, 10, 0, 0, 0)));
}

#[test]
fn cpu_delta_excludes_guest_double_count_and_iowait_from_busy() {
    let start = cpu(100, 0, 100, 100, 50, 10, 0);
    let end = cpu(130, 0, 120, 130, 70, 20, 0);
    // Deltas sum to 110. Guest time is counted in user, so total is 100;
    // idle (30) and iowait (20) leave 50 busy ticks: 50%.
    assert_eq!(cpu_percent(start, end), 50.0);
}

#[test]
fn regressing_cpu_fields_are_clamped_individually_and_zero_total_is_zero() {
    let start = cpu(20, 5, 8, 30, 4, 1, 1);
    let end = cpu(25, 3, 10, 31, 5, 1, 0);
    // The negative nice and guest_nice deltas are zero; guest is also removed
    // from the user delta's total accounting.
    assert_eq!(cpu_percent(start, end), 77.8);
    assert_eq!(cpu_percent(start, start), 0.0);
}

#[test]
fn cpu_one_decimal_rounding_uses_ties_to_even() {
    assert_eq!(crate::round_one_decimal(1.25), 1.2);
    assert_eq!(crate::round_one_decimal(1.35), 1.4);
}

#[test]
fn percentage_rounding_preserves_python_binary_values_before_decimal_ties() {
    for (input, expected) in [
        (0.05, 0.1),
        (0.15, 0.1),
        (0.25, 0.2),
        (0.75, 0.8),
        (1.05, 1.1),
        (1.15, 1.1),
        (3.45, 3.5),
        (-0.05, -0.1),
        (-0.15, -0.1),
        (-0.25, -0.2),
        (-1.05, -1.1),
        (-3.45, -3.5),
        (2.55, 2.5),
        (1e-12, 0.0),
        (100.0, 100.0),
    ] {
        assert_eq!(
            crate::round_one_decimal(input),
            expected,
            "Python round({input}, 1)"
        );
    }
    assert!(crate::round_one_decimal(-1e-12).is_sign_negative());
    let percent = cpu_percent(
        CpuCounters::default(),
        CpuCounters {
            user: 69,
            idle: 1931,
            ..Default::default()
        },
    );
    assert_eq!(percent, 3.5);
    assert_eq!(
        percent.round_ties_even(),
        4.0,
        "HA's second rounding must retain the source CPU value"
    );
    assert_eq!(
        crate::memory::memory_sample(2000, 1949)
            .unwrap()
            .used_percent,
        2.5
    );
}

#[test]
fn linux_cpu_parser_reads_guest_fields_and_defaults_absent_optional_fields() {
    let parsed = parse_cpu_stat("cpu 100 20 30 400 50 6 7 8 9 10\ncpu0 1 2 3 4\n")
        .expect("valid aggregate row");
    assert_eq!(
        parsed,
        CpuCounters {
            user: 100,
            nice: 20,
            system: 30,
            idle: 400,
            iowait: 50,
            irq: 6,
            softirq: 7,
            steal: 8,
            guest: 9,
            guest_nice: 10,
        }
    );

    let old_kernel = parse_cpu_stat("cpu 1 2 3 4\n").expect("required counters");
    assert_eq!(old_kernel.iowait, 0);
    assert_eq!(old_kernel.guest, 0);
    assert!(parse_cpu_stat("cpu 1 nope 3 4\n").is_err());
    assert!(parse_cpu_stat("cpu 1 2 3\n").is_err());
}

fn meminfo(entries: &[(&str, i128)]) -> HashMap<String, i128> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), *value))
        .collect()
}

#[test]
fn linux_memavailable_is_primary_and_negative_or_excess_values_match_psutil_clamps() {
    let base = meminfo(&[("MemTotal:", 100 * 1024), ("MemFree:", 40 * 1024)]);
    let present = HashMap::from([
        ("MemTotal:".to_owned(), 100 * 1024),
        ("MemFree:".to_owned(), 40 * 1024),
        ("MemAvailable:".to_owned(), 25 * 1024),
    ]);
    let sample = memory_from_meminfo(&present, None, 4096).expect("MemAvailable");
    assert_eq!(sample.available_bytes, 25 * 1024);

    let negative = HashMap::from([
        ("MemTotal:".to_owned(), 100 * 1024),
        ("MemFree:".to_owned(), 40 * 1024),
        ("MemAvailable:".to_owned(), -4 * 1024),
    ]);
    assert_eq!(
        memory_from_meminfo(&negative, None, 4096)
            .unwrap()
            .available_bytes,
        0
    );

    let over_total = HashMap::from([
        ("MemTotal:".to_owned(), 100 * 1024),
        ("MemFree:".to_owned(), 40 * 1024),
        ("MemAvailable:".to_owned(), 200 * 1024),
    ]);
    assert_eq!(
        memory_from_meminfo(&over_total, None, 4096)
            .unwrap()
            .available_bytes,
        40 * 1024
    );
    assert!(memory_from_meminfo(&base, None, 4096).is_ok());
}

#[test]
fn linux_missing_or_zero_memavailable_uses_zone_watermark_formula() {
    let entries = meminfo(&[
        ("MemTotal:", 1024),
        ("MemFree:", 200),
        ("Cached:", 80),
        ("Active(file):", 100),
        ("Inactive(file):", 100),
        ("SReclaimable:", 80),
    ]);
    // Low watermarks sum across zones: 15 pages at 4 bytes each => 60 bytes.
    // Page cache is 200 bytes; both adjustments are capped at 60, so avail=320.
    let zoneinfo = "Node 0, zone DMA\n  low 10\n  high 20\nNode 0, zone DMA32\n  low 5\n";
    assert_eq!(
        memory_from_meminfo(&entries, Some(zoneinfo), 4)
            .unwrap()
            .available_bytes,
        320
    );

    let zero_available = HashMap::from([
        ("MemTotal:".to_owned(), 1024),
        ("MemFree:".to_owned(), 200),
        ("MemAvailable:".to_owned(), 0),
        ("Cached:".to_owned(), 80),
        ("Active(file):".to_owned(), 100),
        ("Inactive(file):".to_owned(), 100),
        ("SReclaimable:".to_owned(), 80),
    ]);
    assert_eq!(
        memory_from_meminfo(&zero_available, Some(zoneinfo), 4)
            .unwrap()
            .available_bytes,
        320
    );
}

#[test]
fn linux_fallback_uses_memfree_plus_cached_when_inputs_or_zoneinfo_are_missing() {
    let missing_inputs = meminfo(&[
        ("MemTotal:", 1024),
        ("MemFree:", 200),
        ("Cached:", 80),
        ("Active(file):", 100),
    ]);
    assert_eq!(
        memory_from_meminfo(&missing_inputs, Some("  low 50\n"), 4)
            .unwrap()
            .available_bytes,
        280
    );

    let complete_inputs = meminfo(&[
        ("MemTotal:", 1024),
        ("MemFree:", 200),
        ("Cached:", 80),
        ("Active(file):", 100),
        ("Inactive(file):", 100),
        ("SReclaimable:", 80),
    ]);
    assert_eq!(
        memory_from_meminfo(&complete_inputs, None, 4)
            .unwrap()
            .available_bytes,
        280
    );
}

#[test]
fn linux_fallback_underflow_clamps_to_zero_and_meminfo_errors_propagate() {
    let underflow = meminfo(&[
        ("MemTotal:", 1024),
        ("MemFree:", 1),
        ("Cached:", 0),
        ("Active(file):", 0),
        ("Inactive(file):", 0),
        ("SReclaimable:", 0),
    ]);
    assert_eq!(
        memory_from_meminfo(&underflow, Some("  low 2\n"), 4096)
            .unwrap()
            .available_bytes,
        0
    );
    assert!(parse_meminfo("MemTotal: nope kB\n").is_err());
    assert!(parse_meminfo("malformed\n").is_err());
    assert!(
        memory_from_meminfo(
            &meminfo(&[
                ("MemTotal:", 1024),
                ("MemFree:", 200),
                ("Active(file):", 100),
                ("Inactive(file):", 100),
                ("SReclaimable:", 80),
            ]),
            Some("  low not-a-number\n"),
            4096
        )
        .is_err()
    );
    assert!(memory_from_meminfo(&HashMap::new(), None, 4096).is_err());
}

#[test]
fn linux_meminfo_parsing_preserves_kibibytes_and_last_duplicate_value() {
    let parsed =
        parse_meminfo("MemTotal: 12 kB\nMemFree: 3 kB\nMemFree: 4 kB\n").expect("valid fixture");
    assert_eq!(parsed.get("MemTotal:"), Some(&(12 * 1024)));
    assert_eq!(parsed.get("MemFree:"), Some(&(4 * 1024)));
}

#[test]
fn macos_cpu_maps_host_ticks_to_psutil_time_fields() {
    let parsed = cpu_counters_from_mach([10, 20, 30, 40]);
    assert_eq!(parsed.user, 10);
    assert_eq!(parsed.system, 20);
    assert_eq!(parsed.idle, 30);
    assert_eq!(parsed.nice, 40);
    assert_eq!(parsed.iowait, 0);
    assert_eq!(parsed.guest, 0);
}

#[test]
fn macos_available_is_inactive_plus_free_before_any_speculative_adjustment() {
    let sample = memory_from_vm_pages(
        100 * 4096,
        4096,
        VmPageCounts {
            active: 35,
            inactive: 20,
            free: 30,
            speculative: 10,
        },
    )
    .expect("valid Mach fixture");
    assert_eq!(sample.total_bytes, 100 * 4096);
    assert_eq!(sample.available_bytes, 50 * 4096);
    assert_eq!(sample.used_percent, 50.0);

    let rounded = memory_from_vm_pages(
        400,
        1,
        VmPageCounts {
            active: 0,
            inactive: 395,
            free: 0,
            speculative: 0,
        },
    )
    .expect("fractional memory percentage");
    assert_eq!(rounded.used_percent, 1.2);
}

#[test]
fn memory_read_errors_and_invalid_mach_values_do_not_become_zero() {
    assert!(
        memory_from_vm_pages(
            0,
            4096,
            VmPageCounts {
                active: 0,
                inactive: 0,
                free: 0,
                speculative: 0,
            }
        )
        .is_err()
    );
    assert!(
        memory_from_vm_pages(
            10,
            4096,
            VmPageCounts {
                active: 0,
                inactive: 2,
                free: 2,
                speculative: 0,
            }
        )
        .is_err()
    );
}
