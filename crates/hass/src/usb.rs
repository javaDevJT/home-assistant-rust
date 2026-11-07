//! Strong ownership of native USB enumeration, hotplug, timers and admission.

use crate::{
    discovery::{DiscoveryHandle, DiscoveryRemoval},
    usb_scheduler::UsbScheduler,
};
use anyhow::Result;
use hass_usb::{
    DiscoveryAction, ScanReport, SerialNumberPolicy, SerialPortProvider, UsbCallbackGuard,
    UsbHotplugWatcher, UsbIntegration, UsbManager, UsbOptions, UsbServiceInfo, WatcherStartError,
};
use hass_usb_backend::{NativeSerialPortProvider, NativeSerialPortWatcher};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

const POLL_PERIOD: Duration = Duration::from_secs(5);

/// serialx 1.15.1 `common.is_non_unique_serial`, including FTDI's Windows A
/// suffix. Membership is deliberately case-sensitive and does not trim a
/// nonempty serial before comparison.
struct NativeSerialNumberPolicy;

impl SerialNumberPolicy for NativeSerialNumberPolicy {
    fn is_non_unique_serial(&self, serial: Option<&str>) -> bool {
        let Some(serial) = serial else { return true };
        if serial
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
        {
            return true;
        }
        const FTDI: &[&str] = &[
            "A50285BI", "A5XK3RJT", "A5XK3RJJ", "A5069RR4", "FTB6SPL3", "A601CKQK", "A400DE3U",
            "00000000", "0000000", "DUT", "12345678", "ZIGATE+", "001",
        ];
        const OTHER: &[&str] = &[
            "0001",
            "1234",
            "00000000001A",
            "0",
            "123456",
            "0000000000",
            "EEEEEEEEEEEEEEEE",
            "HIDFG",
            "0123456789ABCDEF",
            "12345678-9012-3456-7890-123456789012",
            "301",
            "304",
            "0123456789",
            "000000000000",
            "00000001",
            "123456789",
            "000000000001",
            "1234567890",
            "000000000000000000000000",
        ];
        OTHER.contains(&serial)
            || FTDI.contains(&serial)
            || serial
                .strip_suffix('A')
                .is_some_and(|base| FTDI.contains(&base))
    }
}

pub struct UsbHost {
    integration: Option<UsbIntegration>,
    scheduler: Arc<UsbScheduler>,
    _callbacks: Vec<UsbCallbackGuard>,
    stop: watch::Sender<bool>,
    poll: Option<JoinHandle<()>>,
}

impl UsbHost {
    pub async fn start(admission: DiscoveryHandle) -> Result<Self> {
        Self::with_providers(
            admission,
            Arc::new(NativeSerialPortProvider),
            Arc::new(NativeSerialPortWatcher::new()),
            UsbOptions::default(),
            POLL_PERIOD,
        )
        .await
    }

    async fn with_providers(
        admission: DiscoveryHandle,
        provider: Arc<dyn SerialPortProvider>,
        watcher: Arc<dyn UsbHotplugWatcher>,
        options: UsbOptions,
        poll_period: Duration,
    ) -> Result<Self> {
        let scheduler = Arc::new(UsbScheduler::new(tokio::runtime::Handle::current()));
        let integration = UsbIntegration::new(
            provider,
            scheduler.clone(),
            options,
            Arc::new(NativeSerialNumberPolicy),
        );
        let manager = integration.manager();
        let discovery = manager.register_discovery_callback(move |action| {
            let outcome = match action {
                DiscoveryAction::Create(create) => admission.create_usb(
                    create.domain.clone(),
                    Map::new(),
                    service_data(&create.data),
                ),
                DiscoveryAction::Dismiss(dismiss) => admission.dismiss(DiscoveryRemoval::Usb {
                    domains: dismiss.matching_domains.clone(),
                    info: service_data(&dismiss.data),
                }),
            };
            outcome.map_err(|error| hass_usb::UsbError::new(error.to_string()))
        })?;
        let error_manager = manager.clone();
        let errors = manager.register_scan_error_callback(move |error| {
            tracing::error!(target: "hass::usb", %error, "USB scan failed");
            // This host consumes the SDK error channel through the callback.
            // Do not accumulate a second unbounded history for periodic errors.
            let _ = error_manager.take_scan_errors();
        })?;
        let start_manager = manager.clone();
        let watcher_result =
            tokio::task::spawn_blocking(move || start_manager.start_watcher(watcher)).await?;
        let fallback = match watcher_result {
            Ok(()) => false,
            Err(WatcherStartError::InotifyNotAvailable(reason)) => {
                tracing::info!(target: "hass::usb", %reason, "USB hotplug unavailable; polling serial ports every five seconds");
                true
            }
            Err(WatcherStartError::Other(error)) => {
                let stopped = tokio::task::spawn_blocking(move || integration.stop()).await;
                scheduler.shutdown().await;
                stopped??;
                return Err(error.into());
            }
        };
        let (stop, mut receiver) = watch::channel(false);
        let poll = fallback.then(|| {
            tokio::spawn(async move {
                let mut deadline = Instant::now() + poll_period;
                loop {
                    tokio::select! {
                        biased;
                        _ = receiver.changed() => break,
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                    // Once admitted, join the blocking enumeration even if
                    // shutdown arrives. Aborting it would detach native work.
                    let scan_manager = manager.clone();
                    match tokio::task::spawn_blocking(move || scan_manager.scan_now()).await {
                        Ok(Ok(report)) => log_report(&report),
                        Ok(Err(error)) => tracing::error!(target: "hass::usb", %error, "USB polling scan failed"),
                        Err(error) => tracing::error!(target: "hass::usb", %error, "USB polling worker failed"),
                    }
                    if *receiver.borrow() { break; }
                    // Keep the periodic cadence without replaying missed ticks.
                    while deadline <= Instant::now() {
                        deadline += poll_period;
                    }
                }
            })
        });
        Ok(Self {
            integration: Some(integration),
            scheduler,
            _callbacks: vec![discovery, errors],
            stop,
            poll,
        })
    }

    pub fn manager(&self) -> UsbManager {
        self.integration
            .as_ref()
            .expect("active USB owner")
            .manager()
    }

    /// Called after the actual homeassistant_started event. There is no USB
    /// persisted discovery key or config-entry-removal rediscovery observer.
    pub async fn mark_started(&self) -> Result<()> {
        let manager = self.manager();
        let report = tokio::task::spawn_blocking(move || manager.scan_now()).await??;
        log_report(&report);
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.stop.send_replace(true);
        let integration = self.integration.take().expect("active USB owner");
        let stopped = tokio::task::spawn_blocking(move || integration.stop()).await;
        let poll_result = match self.poll.take() {
            Some(poll) => poll.await,
            None => Ok(()),
        };
        self.scheduler.shutdown().await;
        stopped??;
        poll_result?;
        Ok(())
    }
}

impl Drop for UsbHost {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        // Ordinary teardown uses shutdown() to await every owner. An early
        // startup failure still stops the SDK (and native watcher) on Drop;
        // an already admitted poll is left owned until its task completes.
    }
}

fn service_data(info: &UsbServiceInfo) -> Map<String, Value> {
    json!({
        "device": info.device,
        "vid": info.vid,
        "pid": info.pid,
        "serial_number": info.serial_number,
        "manufacturer": info.manufacturer,
        "description": info.description,
    })
    .as_object()
    .expect("USB service info object")
    .clone()
}

fn log_report(report: &ScanReport) {
    for failure in &report.serial_ports.scanner_failures {
        tracing::error!(target: "hass::usb", scanner_id = failure.scanner_id, error = %failure.error, "USB serial-port scanner failed");
    }
    for failure in &report.callback_failures {
        tracing::error!(target: "hass::usb", callback_id = failure.callback_id, error = %failure.error, "USB port-event callback failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_config::ConfigEntries;
    use hass_core::{MemoryStore, Runtime};
    use hass_usb::{SerialPortInfo, UsbError};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Watcher {
        unavailable: bool,
        starts: AtomicUsize,
        stops: AtomicUsize,
    }
    impl UsbHotplugWatcher for Watcher {
        fn start(&self, _: Arc<dyn Fn() + Send + Sync>) -> Result<(), WatcherStartError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.unavailable {
                Err(WatcherStartError::InotifyNotAvailable("fixture".into()))
            } else {
                Ok(())
            }
        }
        fn stop_and_join(&self) -> Result<(), UsbError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn dispatcher() -> crate::discovery::DiscoveryDispatcher {
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        crate::discovery::DiscoveryDispatcher::new(ConfigEntries::new(runtime).unwrap())
    }

    #[test]
    fn native_serial_policy_retains_serialx_placeholder_and_exact_membership() {
        let policy = NativeSerialNumberPolicy;
        for value in [
            None,
            Some(""),
            Some(" \t\u{2003}"),
            Some("A50285BI"),
            Some("A50285BIA"),
            Some("001A"),
            Some("HIDFG"),
            Some("00000000001A"),
        ] {
            assert!(policy.is_non_unique_serial(value), "{value:?}");
        }
        for value in [
            "a50285bi",
            " A50285BI ",
            "HIDFGA",
            "fixture-unique-123",
            "0001A",
        ] {
            assert!(!policy.is_non_unique_serial(Some(value)), "{value}");
        }
    }

    #[tokio::test]
    async fn started_scan_and_owned_watcher_stop_use_only_injected_provider() {
        let dispatcher = dispatcher();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let provider = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::<SerialPortInfo>::new())
        });
        let watcher = Arc::new(Watcher {
            unavailable: false,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let host = UsbHost::with_providers(
            dispatcher.handle(),
            provider,
            watcher.clone(),
            UsbOptions::default(),
            POLL_PERIOD,
        )
        .await
        .unwrap();
        let manager = host.manager();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        host.mark_started().await.unwrap();
        assert!(manager.initial_scan_done().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let request_manager = manager.clone();
        tokio::task::spawn_blocking(move || request_manager.request_scan())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "active watcher suppresses manual scans"
        );
        host.shutdown().await.unwrap();
        assert!(manager.scan_serial_ports().is_err());
        assert_eq!(watcher.starts.load(Ordering::SeqCst), 1);
        assert_eq!(watcher.stops.load(Ordering::SeqCst), 1);
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn usb_scan_bridge_queues_keyless_flows_and_removes_exact_device() {
        use hass_config::{
            ConfigEntry, ConfigEntryDriver, ConfigFlowRequest, ConfigFlowResult, SetupOutcome,
        };
        struct Driver(Mutex<Vec<ConfigFlowRequest>>);
        impl ConfigEntryDriver for Driver {
            fn domain(&self) -> &str {
                "usb_fixture"
            }
            fn setup(&self, _: &Runtime, _: &ConfigEntry) -> hass_core::Result<SetupOutcome> {
                Ok(SetupOutcome::Ready)
            }
            fn unload(&self, _: &Runtime, _: &ConfigEntry) -> hass_core::Result<()> {
                Ok(())
            }
            fn config_flow(
                &self,
                request: &ConfigFlowRequest,
            ) -> hass_core::Result<ConfigFlowResult> {
                self.0.lock().unwrap().push(request.clone());
                Ok(ConfigFlowResult::form("confirm", None))
            }
        }
        let runtime = Runtime::new(Arc::new(MemoryStore::default())).unwrap();
        let entries = ConfigEntries::new(runtime).unwrap();
        let driver = Arc::new(Driver(Mutex::new(Vec::new())));
        entries.register_driver(driver.clone()).unwrap();
        let dispatcher = crate::discovery::DiscoveryDispatcher::new(entries.clone());
        let ports = Arc::new(Mutex::new(vec![SerialPortInfo {
            device: "/dev/owned-usb-fixture".into(),
            vid: Some(0x1234),
            pid: Some(0xabcd),
            ..Default::default()
        }]));
        let scanned = ports.clone();
        let provider = Arc::new(move || Ok(scanned.lock().unwrap().clone()));
        let watcher = Arc::new(Watcher {
            unavailable: false,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let host = UsbHost::with_providers(
            dispatcher.handle(),
            provider,
            watcher,
            UsbOptions::new(vec![hass_usb::USBMatcher::new("usb_fixture")]),
            POLL_PERIOD,
        )
        .await
        .unwrap();
        host.mark_started().await.unwrap();
        assert!(
            driver.0.lock().unwrap().is_empty(),
            "flows wait for the shared started boundary"
        );
        dispatcher.mark_started().await;
        let request = driver.0.lock().unwrap()[0].clone();
        assert_eq!(request.source, "usb");
        assert!(!request.context.contains_key("discovery_key"));
        let data = request.user_input.unwrap();
        assert_eq!(data, json!({"device":"/dev/owned-usb-fixture","vid":"1234","pid":"ABCD","serial_number":null,"manufacturer":null,"description":null}).as_object().unwrap().clone());
        assert_eq!(entries.snapshot_with_discovery_flows("usb").1.len(), 1);
        ports.lock().unwrap().clear();
        let manager = host.manager();
        tokio::task::spawn_blocking(move || manager.scan_now())
            .await
            .unwrap()
            .unwrap();
        dispatcher.mark_started().await;
        assert!(entries.flow_result(&request.flow_id).is_err());
        host.shutdown().await.unwrap();
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn unavailable_watcher_polls_and_shutdown_joins_admitted_provider() {
        let dispatcher = dispatcher();
        let (started, start_receiver) = tokio::sync::oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started)));
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Mutex::new(blocked);
        let provider = Arc::new(move || {
            if let Some(sender) = started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            blocked.lock().unwrap().recv().unwrap();
            Ok(Vec::<SerialPortInfo>::new())
        });
        let watcher = Arc::new(Watcher {
            unavailable: true,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        });
        let host = UsbHost::with_providers(
            dispatcher.handle(),
            provider,
            watcher,
            UsbOptions::default(),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), start_receiver)
            .await
            .unwrap()
            .unwrap();
        let manager = host.manager();
        let stop = tokio::spawn(host.shutdown());
        tokio::task::yield_now().await;
        assert!(!stop.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(manager.scan_serial_ports().is_err());
        dispatcher.shutdown().await;
    }
}
