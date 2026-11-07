use crate::HardwareProvider;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::broadcast;

use chrono::{TimeZone, Utc};
use serde_json::json;
use tokio::sync::Notify;

use crate::{
    BoardInfo, HardwareConfig, HardwareInfo, HardwareIntegration, MetricsProvider,
    RawSystemMetrics, SystemStatus, USBInfo,
};

const MIB: u64 = 1024 * 1024;

fn metrics() -> RawSystemMetrics {
    RawSystemMetrics {
        cpu_percent: 12.4,
        total_bytes: 8 * MIB,
        available_bytes: 3 * MIB,
        memory_used_percent: 62.5,
    }
}

fn metrics_provider() -> Arc<dyn MetricsProvider> {
    Arc::new(|| Ok(metrics()))
}

fn named_hardware(name: &str) -> HardwareInfo {
    HardwareInfo {
        name: Some(name.to_owned()),
        board: None,
        config_entries: None,
        dongle: None,
        url: None,
    }
}

fn fast_config() -> HardwareConfig {
    HardwareConfig {
        sample_interval: Duration::from_millis(100),
        status_channel_capacity: 8,
    }
}

#[test]
fn hardware_models_keep_nullable_fields_in_serialized_shape() {
    let record = HardwareInfo {
        name: None,
        board: Some(BoardInfo {
            hassio_board_id: None,
            manufacturer: "board-maker".to_owned(),
            model: None,
            revision: None,
        }),
        config_entries: None,
        dongle: Some(USBInfo {
            vid: "10c4".to_owned(),
            pid: "ea60".to_owned(),
            serial_number: None,
            manufacturer: None,
            description: None,
        }),
        url: None,
    };

    assert_eq!(
        serde_json::to_value(record).unwrap(),
        json!({
            "name": null,
            "board": {
                "hassio_board_id": null,
                "manufacturer": "board-maker",
                "model": null,
                "revision": null
            },
            "config_entries": null,
            "dongle": {
                "vid": "10c4",
                "pid": "ea60",
                "serial_number": null,
                "manufacturer": null,
                "description": null
            },
            "url": null
        })
    );
}

#[test]
fn system_status_uses_ties_to_even_and_preserves_provider_memory_percent() {
    let timestamp = Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap();
    let raw = RawSystemMetrics {
        cpu_percent: 2.5,
        total_bytes: 5 * MIB / 2,
        available_bytes: 5 * MIB / 4,
        memory_used_percent: 37.123,
    };
    let status = SystemStatus::from_metrics_at(raw, timestamp).unwrap();

    assert_eq!(status.cpu_percent, 2);
    assert_eq!(status.memory_used_percent, 37.123);
    assert_eq!(status.memory_used_mb, 1.2);
    assert_eq!(status.memory_free_mb, 1.2);
    assert_eq!(status.timestamp, "2026-11-07T12:00:00+00:00");
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({
            "cpu_percent": 2,
            "memory_used_percent": 37.123,
            "memory_used_mb": 1.2,
            "memory_free_mb": 1.2,
            "timestamp": "2026-11-07T12:00:00+00:00"
        })
    );

    let odd_tie = SystemStatus::from_metrics_at(
        RawSystemMetrics {
            cpu_percent: 1.5,
            ..raw
        },
        timestamp,
    )
    .unwrap();
    assert_eq!(odd_tie.cpu_percent, 2);
}

#[test]
fn system_status_rejects_invalid_metrics_instead_of_fabricating_values() {
    assert!(
        SystemStatus::from_metrics(RawSystemMetrics {
            total_bytes: 0,
            available_bytes: 0,
            ..metrics()
        })
        .is_err()
    );
    assert!(
        SystemStatus::from_metrics(RawSystemMetrics {
            cpu_percent: f64::NAN,
            ..metrics()
        })
        .is_err()
    );
    assert!(
        SystemStatus::from_metrics(RawSystemMetrics {
            available_bytes: 9 * MIB,
            ..metrics()
        })
        .is_err()
    );
}

#[test]
fn status_timestamp_keeps_python_datetime_microsecond_precision() {
    for (nanos, suffix) in [
        (0, "00+00:00"),
        (1_000_000, "00.001000+00:00"),
        (123_456_789, "00.123456+00:00"),
    ] {
        let timestamp = Utc.timestamp_opt(0, nanos).unwrap();
        assert_eq!(
            SystemStatus::from_metrics_at(metrics(), timestamp)
                .unwrap()
                .timestamp,
            format!("1970-01-01T00:00:{suffix}")
        );
    }
}

#[tokio::test]
async fn no_loaded_provider_is_an_empty_success_and_provider_guards_are_exact() {
    let mut integration = HardwareIntegration::new(metrics_provider()).unwrap();
    let manager = integration.manager();

    assert!(manager.hardware_info().await.unwrap().is_empty());

    let old = manager
        .register_provider("board", Arc::new(|| Ok(vec![named_hardware("old")])))
        .unwrap();
    let replacement = manager
        .register_provider("board", Arc::new(|| Ok(vec![named_hardware("new")])))
        .unwrap();
    drop(old);
    assert_eq!(
        manager.hardware_info().await.unwrap(),
        vec![named_hardware("new")]
    );

    drop(replacement);
    assert!(manager.hardware_info().await.unwrap().is_empty());
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_errors_and_panics_are_isolated_to_their_loaded_provider() {
    let mut integration = HardwareIntegration::new(metrics_provider()).unwrap();
    let manager = integration.manager();
    let _error = manager
        .register_provider(
            "not_ready",
            Arc::new(|| Err(hass_core::Error::HomeAssistant("not ready".to_owned()))),
        )
        .unwrap();
    let _panic = manager
        .register_provider(
            "panics",
            Arc::new(|| -> hass_core::Result<Vec<HardwareInfo>> {
                panic!("provider panic must be contained")
            }),
        )
        .unwrap();
    let _ok = manager
        .register_provider("loaded", Arc::new(|| Ok(vec![named_hardware("real row")])))
        .unwrap();

    assert_eq!(
        manager.hardware_info().await.unwrap(),
        vec![named_hardware("real row")]
    );
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn manager_is_weak_and_operations_fail_after_owner_shutdown() {
    let mut integration = HardwareIntegration::new(metrics_provider()).unwrap();
    let manager = integration.manager();
    integration.shutdown().await.unwrap();

    assert!(manager.hardware_info().await.is_err());
    assert!(
        manager
            .register_provider("late", Arc::new(|| Ok(Vec::new())))
            .is_err()
    );
    assert!(manager.subscribe_system_status().is_err());
}

#[tokio::test]
async fn subscribers_share_delayed_sampling_and_last_unsubscribe_stops_it() {
    let reads = Arc::new(AtomicUsize::new(0));
    let read_count = Arc::clone(&reads);
    let metrics_provider: Arc<dyn MetricsProvider> = Arc::new(move || {
        read_count.fetch_add(1, Ordering::SeqCst);
        Ok(metrics())
    });
    let mut config = fast_config();
    config.sample_interval = Duration::from_millis(120);
    let mut integration =
        HardwareIntegration::with_config(metrics_provider, config.clone()).unwrap();
    let manager = integration.manager();

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let mut first = manager.subscribe_system_status().unwrap();
    let mut second = manager.subscribe_system_status().unwrap();
    assert!(matches!(
        first.receiver().try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    tokio::time::timeout(Duration::from_secs(2), first.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), second.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    drop(first);
    tokio::time::timeout(Duration::from_secs(2), second.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    drop(second);
    tokio::time::sleep(config.sample_interval * 2).await;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn zero_to_one_generation_does_not_deliver_an_old_in_flight_read() {
    let started = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    let read_started = Arc::clone(&started);
    let read_finished = Arc::clone(&finished);
    let read_gate = Arc::clone(&gate);
    let read_count = Arc::clone(&reads);
    let metrics_provider: Arc<dyn MetricsProvider> = Arc::new(move || {
        read_count.fetch_add(1, Ordering::SeqCst);
        read_started.notify_one();
        let (released, changed) = &*read_gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        read_finished.notify_one();
        Ok(metrics())
    });
    let mut config = fast_config();
    config.sample_interval = Duration::from_millis(180);
    let mut integration = HardwareIntegration::with_config(metrics_provider, config).unwrap();
    let manager = integration.manager();

    let old_subscriber = manager.subscribe_system_status().unwrap();
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    drop(old_subscriber);
    let mut new_subscriber = manager.subscribe_system_status().unwrap();
    let timer_begun = std::time::Instant::now();

    {
        let (released, changed) = &*gate;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), finished.notified())
        .await
        .unwrap();

    let too_early = tokio::time::timeout(Duration::from_millis(40), new_subscriber.recv()).await;
    assert!(
        too_early.is_err(),
        "new subscriber received an old or immediate sample"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);

    tokio::time::timeout(Duration::from_secs(2), new_subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(timer_begun.elapsed() >= Duration::from_millis(150));
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    drop(new_subscriber);
    integration.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn status_read_duration_does_not_delay_the_next_interval() {
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let observed_gate = gate.clone();
    let provider: Arc<dyn MetricsProvider> = Arc::new(move || {
        started.send(()).unwrap();
        let (released, changed) = &*observed_gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        Ok(metrics())
    });
    let mut config = fast_config();
    config.sample_interval = Duration::from_millis(100);
    let mut integration = HardwareIntegration::with_config(provider, config).unwrap();
    let mut subscription = integration.manager().subscribe_system_status().unwrap();
    let begun = tokio::time::Instant::now();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    starts.recv().await.unwrap();
    tokio::time::advance(Duration::from_millis(70)).await;
    {
        let (released, changed) = &*gate;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }
    subscription.recv().await.unwrap();
    tokio::time::advance(Duration::from_millis(29)).await;
    assert!(matches!(
        starts.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    tokio::time::advance(Duration::from_millis(1)).await;
    // A real-time timeout would auto-advance this paused clock. Await only the
    // already-admitted blocking worker, then check the virtual instant.
    starts.recv().await.unwrap();
    assert_eq!(
        tokio::time::Instant::now().duration_since(begun),
        Duration::from_millis(200)
    );
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_shutdown_closes_subscriptions_while_owner_is_retained() {
    let mut integration = HardwareIntegration::new(metrics_provider()).unwrap();
    let manager = integration.manager();
    let mut subscription = manager.subscribe_system_status().unwrap();
    let mut raw_subscription = manager.subscribe_system_status().unwrap();
    integration.shutdown().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .unwrap(),
        Err(broadcast::error::RecvError::Closed)
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), raw_subscription.receiver().recv())
            .await
            .unwrap(),
        Err(broadcast::error::RecvError::Closed)
    ));
    assert!(manager.subscribe_system_status().is_err());
}

#[tokio::test]
async fn cancelled_info_request_remains_owned_until_shutdown_joins_provider() {
    let mut integration = HardwareIntegration::new(metrics_provider()).unwrap();
    let manager = integration.manager();
    let started = Arc::new(Notify::new());
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let observed_started = started.clone();
    let observed_gate = gate.clone();
    let provider: Arc<dyn HardwareProvider> = Arc::new(move || {
        observed_started.notify_one();
        let (released, changed) = &*observed_gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        Ok(Vec::new())
    });
    let _guard = manager
        .register_provider("owned_fixture", provider)
        .unwrap();
    let info = tokio::spawn(async move { manager.hardware_info().await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    info.abort();
    assert!(info.await.unwrap_err().is_cancelled());
    let shutdown = tokio::spawn(async move { integration.shutdown().await });
    tokio::task::yield_now().await;
    let completed_early = shutdown.is_finished();
    {
        let (released, changed) = &*gate;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !completed_early,
        "cancelled caller must not detach admitted provider work"
    );
}

#[tokio::test]
async fn invalid_zero_memory_and_failed_metrics_are_omitted_before_recovery() {
    let reads = Arc::new(AtomicUsize::new(0));
    let calls = reads.clone();
    let provider: Arc<dyn MetricsProvider> =
        Arc::new(move || match calls.fetch_add(1, Ordering::SeqCst) {
            0 => Err(hass_core::Error::Service("injected metrics error".into())),
            1 => panic!("injected metrics panic"),
            2 => Ok(RawSystemMetrics {
                total_bytes: 0,
                available_bytes: 0,
                cpu_percent: 99.0,
                ..metrics()
            }),
            _ => Ok(RawSystemMetrics {
                cpu_percent: 27.0,
                ..metrics()
            }),
        });
    let mut config = fast_config();
    config.sample_interval = Duration::from_millis(10);
    let mut integration = HardwareIntegration::with_config(provider, config).unwrap();
    let mut subscription = integration.manager().subscribe_system_status().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.cpu_percent, 27);
    assert!(reads.load(Ordering::SeqCst) >= 4);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_an_admitted_blocking_metrics_read() {
    let started = Arc::new(Notify::new());
    let gate = Arc::new((StdMutex::new(false), Condvar::new()));
    let read_started = Arc::clone(&started);
    let read_gate = Arc::clone(&gate);
    let metrics_provider: Arc<dyn MetricsProvider> = Arc::new(move || {
        read_started.notify_one();
        let (released, changed) = &*read_gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        Ok(metrics())
    });
    let mut config = fast_config();
    config.sample_interval = Duration::from_millis(10);
    let mut integration = HardwareIntegration::with_config(metrics_provider, config).unwrap();
    let manager = integration.manager();
    let _subscription = manager.subscribe_system_status().unwrap();

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let shutdown = tokio::spawn(async move { integration.shutdown().await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown returned before read completion"
    );

    {
        let (released, changed) = &*gate;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }
    shutdown.await.unwrap().unwrap();
}

#[tokio::test]
async fn lagged_status_is_observable_and_queue_is_bounded() {
    let reads = Arc::new(AtomicUsize::new(0));
    let read_count = Arc::clone(&reads);
    let metrics_provider: Arc<dyn MetricsProvider> = Arc::new(move || {
        read_count.fetch_add(1, Ordering::SeqCst);
        Ok(metrics())
    });
    let config = HardwareConfig {
        sample_interval: Duration::from_millis(20),
        status_channel_capacity: 1,
    };
    let mut integration = HardwareIntegration::with_config(metrics_provider, config).unwrap();
    let manager = integration.manager();
    let mut subscription = manager.subscribe_system_status().unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        while reads.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    assert!(matches!(
        subscription.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    drop(subscription);
    integration.shutdown().await.unwrap();
}

#[tokio::test]
async fn config_rejects_zero_interval_and_zero_queue_capacity() {
    let mut config = fast_config();
    config.sample_interval = Duration::ZERO;
    assert!(HardwareIntegration::with_config(metrics_provider(), config).is_err());

    let mut config = fast_config();
    config.status_channel_capacity = 0;
    assert!(HardwareIntegration::with_config(metrics_provider(), config).is_err());
}
