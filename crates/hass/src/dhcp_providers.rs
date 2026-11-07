//! Owned native DHCP capture and active-scan providers.
//!
//! Capture readers run on foreign threads. They only enqueue decoded wire
//! observations here; one Tokio consumer performs cache and discovery-flow
//! work in input order. `DhcpProviders::shutdown` stops and joins capture
//! readers before closing and draining that queue, and joins the active scan
//! task before the DHCP SDK owner may be dropped.

use anyhow::{Result, anyhow};
use hass_dhcp::{DhcpManager, DhcpObservation};
#[cfg(any(target_os = "linux", test))]
use hass_dhcp_discover::ScanConfig;
use hass_dhcp_discover::{NetworkProvider, PtrResolver, ScanOutput, ScanStatus, Scanner};
use hass_dhcp_wire::{
    CaptureBackend, CaptureConfig, CaptureHandle, InterfaceSelector, PnetCaptureBackend,
    WireObservation, start_capture,
};
use hass_network::{NetworkAdapter, NetworkManager};
use std::collections::HashSet;
#[cfg(any(target_os = "linux", test))]
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const SCAN_INTERVAL: Duration = Duration::from_secs(60 * 60);
const CAPTURE_QUEUE_CAPACITY: usize = 256;

/// Owner for native DHCP observations and hourly active scans.
pub struct DhcpProviders {
    capture_handles: Vec<CaptureHandle>,
    observations_tx: mpsc::Sender<WireObservation>,
    observation_task: JoinHandle<()>,
    scan_task: Option<ScanTask>,
    capture_overflows: Arc<AtomicU64>,
}

struct ScanTask {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl DhcpProviders {
    /// Start native providers after Home Assistant has reached its started event.
    pub async fn start(manager: DhcpManager, network: NetworkManager) -> Result<Self> {
        let adapters = match network.adapters() {
            Ok(adapters) => adapters,
            Err(error) => {
                tracing::warn!(%error, "Could not load selected adapters for DHCP providers");
                Vec::new()
            }
        };

        let scanner: Option<
            Scanner<hass_dhcp_discover::SystemNetworkProvider, hass_dhcp_discover::UdpPtrResolver>,
        >;
        #[cfg(target_os = "linux")]
        {
            let local_ipv4 = selected_local_ipv4(&adapters);
            scanner = local_ipv4.map(|local_ipv4| {
                Scanner::new(
                    Arc::new(hass_dhcp_discover::SystemNetworkProvider),
                    Arc::new(hass_dhcp_discover::UdpPtrResolver),
                    ScanConfig {
                        local_ipv4: Some(local_ipv4),
                        ..ScanConfig::default()
                    },
                )
            });
            if scanner.is_none() {
                tracing::warn!(
                    "DHCP active scanning is unavailable because no selected adapter has a local IPv4 address"
                );
            }
        }
        #[cfg(target_os = "macos")]
        {
            scanner = None;
            tracing::warn!("DHCP active scanning is unsupported on macOS");
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            scanner = None;
            tracing::warn!("DHCP active scanning is unsupported on this platform");
        }

        Self::start_with(
            manager,
            adapters,
            Arc::new(PnetCaptureBackend),
            scanner,
            SCAN_INTERVAL,
        )
        .await
    }

    /// Test and embedding seam. The caller supplies a snapshot and providers,
    /// so this constructor performs no operating-system network discovery.
    pub(crate) async fn start_with<B, N, R>(
        manager: DhcpManager,
        adapters: Vec<NetworkAdapter>,
        backend: Arc<B>,
        scanner: Option<Scanner<N, R>>,
        scan_interval: Duration,
    ) -> Result<Self>
    where
        B: CaptureBackend,
        N: NetworkProvider,
        R: PtrResolver,
    {
        if scan_interval.is_zero() {
            return Err(anyhow!("DHCP scan interval must be greater than zero"));
        }

        let (observations_tx, observations_rx) = mpsc::channel(CAPTURE_QUEUE_CAPACITY);
        let capture_overflows = Arc::new(AtomicU64::new(0));
        let observation_task = spawn_observation_consumer(manager.clone(), observations_rx);
        let capture_handles = start_captures(
            adapters.as_slice(),
            backend,
            observations_tx.clone(),
            Arc::clone(&capture_overflows),
        )
        .await;
        let scan_task = scanner.map(|scanner| spawn_scan_task(scanner, manager, scan_interval));

        Ok(Self {
            capture_handles,
            observations_tx,
            observation_task,
            scan_task,
            capture_overflows,
        })
    }

    /// Stop providers and join all reader threads and Tokio tasks.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            capture_handles,
            observations_tx,
            observation_task,
            scan_task,
            capture_overflows,
        } = self;

        // Signal every owned worker before awaiting any one of them.
        if let Some(task) = &scan_task {
            task.stop.send_replace(true);
        }
        for capture in &capture_handles {
            capture.request_stop();
        }

        let mut first_error: Option<anyhow::Error> = None;
        if let Some(task) = scan_task
            && let Err(error) = task.join.await
        {
            tracing::error!(%error, "DHCP active scan task failed while joining");
            first_error.get_or_insert_with(|| anyhow!("DHCP active scan task failed: {error}"));
        }

        // The stop flag was sent to all readers above. Await each handle so no
        // callback can race the cache owner after this method returns.
        for capture in capture_handles {
            if let Err(error) = capture.shutdown_async().await {
                tracing::warn!(%error, "DHCP capture worker failed while shutting down");
                first_error.get_or_insert_with(|| anyhow!("DHCP capture shutdown failed: {error}"));
            }
        }

        // All reader closures are gone now. Closing the final sender lets the
        // serial consumer drain accepted observations before it exits.
        drop(observations_tx);
        if let Err(error) = observation_task.await {
            tracing::error!(%error, "DHCP observation consumer failed while joining");
            first_error.get_or_insert_with(|| anyhow!("DHCP observation consumer failed: {error}"));
        }

        let overflow_count = capture_overflows.load(Ordering::Relaxed);
        if overflow_count > 0 {
            tracing::warn!(
                dropped_observations = overflow_count,
                "DHCP capture queue dropped observations while it was full"
            );
        }

        first_error.map_or(Ok(()), Err)
    }
}

fn selected_capture_interfaces(adapters: &[NetworkAdapter]) -> Vec<InterfaceSelector> {
    let mut seen = HashSet::new();
    adapters
        .iter()
        .filter(|adapter| adapter.enabled)
        .filter_map(|adapter| {
            let index = adapter.index.filter(|index| *index > 0)?;
            let has_local_ipv4 = adapter.ipv4.iter().any(|record| {
                !record.address.is_loopback() && !crate::ip_policy::ipv4_is_global(record.address)
            });
            (has_local_ipv4 && seen.insert(index)).then_some(InterfaceSelector::Index(index))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn selected_local_ipv4(adapters: &[NetworkAdapter]) -> Option<Ipv4Addr> {
    let eligible = |adapter: &&NetworkAdapter| {
        adapter.enabled
            && adapter.ipv4.iter().any(|record| {
                !record.address.is_loopback() && !crate::ip_policy::ipv4_is_global(record.address)
            })
    };
    adapters
        .iter()
        .filter(eligible)
        .find(|adapter| adapter.default)
        .or_else(|| adapters.iter().filter(eligible).next())
        .and_then(|adapter| {
            adapter
                .ipv4
                .iter()
                .find(|record| {
                    !record.address.is_loopback()
                        && !crate::ip_policy::ipv4_is_global(record.address)
                })
                .map(|record| record.address)
        })
}

async fn start_captures<B>(
    adapters: &[NetworkAdapter],
    backend: Arc<B>,
    observations_tx: mpsc::Sender<WireObservation>,
    overflows: Arc<AtomicU64>,
) -> Vec<CaptureHandle>
where
    B: CaptureBackend,
{
    let selectors = selected_capture_interfaces(adapters);
    // Passing an empty selector list to the wire owner creates an idle handle
    // without enumerating or opening any interface.
    let selections = if selectors.is_empty() {
        vec![Vec::new()]
    } else {
        selectors
            .into_iter()
            .map(|selector| vec![selector])
            .collect()
    };

    let mut handles = Vec::new();
    for selection in selections {
        let label = selection
            .first()
            .map(|selector| format!("{selector:?}"))
            .unwrap_or_else(|| "<idle>".to_owned());
        let backend = Arc::clone(&backend);
        let sender = observations_tx.clone();
        let dropped = Arc::clone(&overflows);
        let started = tokio::task::spawn_blocking(move || {
            start_capture(backend, CaptureConfig::new(selection), move |observation| {
                enqueue_observation(&sender, &dropped, observation)
            })
        })
        .await;

        match started {
            Ok(Ok(handle)) => handles.push(handle),
            Ok(Err(error)) => {
                tracing::warn!(interface = %label, %error, "Could not start DHCP capture on selected interface");
            }
            Err(error) => {
                tracing::warn!(interface = %label, %error, "DHCP capture setup task failed");
            }
        }
    }
    handles
}

fn enqueue_observation(
    sender: &mpsc::Sender<WireObservation>,
    overflows: &AtomicU64,
    observation: WireObservation,
) {
    match sender.try_send(observation) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            let dropped = overflows.fetch_add(1, Ordering::Relaxed) + 1;
            // Expose overload while bounding repetitive log volume.
            if dropped == 1 || dropped.is_power_of_two() {
                tracing::warn!(
                    dropped_observations = dropped,
                    "DHCP capture observation queue is full"
                );
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!("DHCP capture observation consumer is closed");
        }
    }
}

fn spawn_observation_consumer(
    manager: DhcpManager,
    mut observations: mpsc::Receiver<WireObservation>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(observation) = observations.recv().await {
            let report = DhcpObservation {
                ip: observation.ip,
                hostname: observation.hostname,
                mac: observation.mac,
            };
            if let Err(error) = manager.ingest(report, false) {
                tracing::warn!(%error, "Could not ingest captured DHCP observation");
            }
        }
    })
}

fn spawn_scan_task<N, R>(
    scanner: Scanner<N, R>,
    manager: DhcpManager,
    scan_interval: Duration,
) -> ScanTask
where
    N: NetworkProvider,
    R: PtrResolver,
{
    let (stop, cancellation) = watch::channel(false);
    let join = tokio::spawn(run_scan_schedule(
        scanner,
        manager,
        scan_interval,
        cancellation,
    ));
    ScanTask { stop, join }
}

async fn run_scan_schedule<N, R>(
    scanner: Scanner<N, R>,
    manager: DhcpManager,
    scan_interval: Duration,
    mut cancellation: watch::Receiver<bool>,
) where
    N: NetworkProvider,
    R: PtrResolver,
{
    // The first deadline is immediate. Keep future deadlines aligned to it,
    // advancing past all ticks missed by a long scan before sleeping again.
    let mut next_tick = Instant::now();

    loop {
        if *cancellation.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(next_tick) => {
                let completed_tick = next_tick;
                match scanner.scan(&mut cancellation).await {
                    Ok(output) => {
                        if !apply_scan_output(&manager, output) {
                            break;
                        }
                    }
                    Err(error) => tracing::warn!(%error, "DHCP active scan failed"),
                }
                next_tick = next_future_tick(completed_tick, scan_interval, Instant::now());
            }
        }
    }
}

fn next_future_tick(previous_tick: Instant, cadence: Duration, now: Instant) -> Instant {
    let mut next = previous_tick
        .checked_add(cadence)
        .unwrap_or_else(|| now + cadence);
    while next <= now {
        next = next.checked_add(cadence).unwrap_or_else(|| now + cadence);
    }
    next
}

/// Apply observations in scan order. `false` indicates a cancelled scan and
/// tells the owning schedule to exit.
fn apply_scan_output(manager: &DhcpManager, output: ScanOutput) -> bool {
    let status = output.status;
    let count = output.observations.len();
    for observation in output.observations {
        if let Err(error) = manager.ingest(
            DhcpObservation {
                ip: observation.ip.to_string(),
                hostname: observation.hostname,
                mac: observation.mac,
            },
            false,
        ) {
            tracing::warn!(%error, "Could not ingest DHCP scan observation");
        }
    }

    match status {
        ScanStatus::Completed => {
            tracing::debug!(observations = count, "DHCP active scan completed")
        }
        ScanStatus::SkippedOversize { address_count } => {
            tracing::debug!(
                address_count,
                "DHCP active scan skipped an oversized subnet"
            );
        }
        ScanStatus::SkippedOverlap => {
            tracing::debug!("DHCP active scan skipped because another scan is running");
        }
        ScanStatus::Cancelled => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use hass_dhcp::{DhcpIntegration, DhcpServiceInfo};
    use hass_dhcp_discover::{NeighborEntry, NetworkSnapshot, ScanError};
    use hass_dhcp_wire::{CaptureInterface, CaptureSource};
    use hass_network::Ipv4AddressRecord;
    use std::collections::HashMap;
    use std::io;
    use std::net::IpAddr;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use tokio::sync::Notify;

    struct FixtureCaptureBackend {
        interfaces: Vec<CaptureInterface>,
        sources: Mutex<HashMap<u32, Receiver<Vec<u8>>>>,
        senders: HashMap<u32, Sender<Vec<u8>>>,
        opened: Mutex<Vec<u32>>,
        enumerate_calls: AtomicUsize,
    }

    impl FixtureCaptureBackend {
        fn new(indices: &[u32]) -> Self {
            let mut interfaces = Vec::new();
            let mut sources = HashMap::new();
            let mut senders = HashMap::new();
            for index in indices {
                let (sender, receiver) = mpsc::channel();
                let name = format!("fixture{index}");
                interfaces.push(CaptureInterface {
                    name,
                    index: *index,
                });
                sources.insert(*index, receiver);
                senders.insert(*index, sender);
            }
            Self {
                interfaces,
                sources: Mutex::new(sources),
                senders,
                opened: Mutex::new(Vec::new()),
                enumerate_calls: AtomicUsize::new(0),
            }
        }
    }

    impl CaptureBackend for FixtureCaptureBackend {
        fn interfaces(&self) -> io::Result<Vec<CaptureInterface>> {
            self.enumerate_calls.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(self.interfaces.clone())
        }

        fn open_source(
            &self,
            interface: &CaptureInterface,
            _read_timeout: Duration,
        ) -> io::Result<Box<dyn CaptureSource>> {
            self.opened.lock().unwrap().push(interface.index);
            let receiver = self
                .sources
                .lock()
                .unwrap()
                .remove(&interface.index)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "fixture source missing"))?;
            Ok(Box::new(QueueSource { receiver }))
        }
    }

    struct QueueSource {
        receiver: Receiver<Vec<u8>>,
    }

    impl CaptureSource for QueueSource {
        fn recv_frame(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
            match self.receiver.recv_timeout(timeout) {
                Ok(frame) => Ok(Some(frame)),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(None),
            }
        }
    }

    fn adapter(
        name: &str,
        index: Option<u32>,
        enabled: bool,
        default: bool,
        addresses: &[Ipv4Addr],
    ) -> NetworkAdapter {
        NetworkAdapter {
            name: name.to_owned(),
            index,
            enabled,
            auto: enabled,
            default,
            ipv4: addresses
                .iter()
                .copied()
                .map(|address| Ipv4AddressRecord {
                    address,
                    network_prefix: 24,
                })
                .collect(),
            ipv6: Vec::new(),
        }
    }

    #[test]
    fn capture_selection_uses_only_enabled_local_indexed_adapters_and_deduplicates() {
        let adapters = vec![
            adapter(
                "private",
                Some(2),
                true,
                false,
                &[Ipv4Addr::new(10, 0, 0, 2)],
            ),
            adapter(
                "same-index",
                Some(2),
                true,
                false,
                &[Ipv4Addr::new(192, 168, 1, 2)],
            ),
            adapter(
                "disabled",
                Some(3),
                false,
                false,
                &[Ipv4Addr::new(10, 0, 0, 3)],
            ),
            adapter("global", Some(4), true, false, &[Ipv4Addr::new(8, 8, 8, 8)]),
            adapter("loopback", Some(5), true, false, &[Ipv4Addr::LOCALHOST]),
            adapter("no-index", None, true, false, &[Ipv4Addr::new(10, 0, 0, 5)]),
            adapter(
                "zero-index",
                Some(0),
                true,
                false,
                &[Ipv4Addr::new(10, 0, 0, 6)],
            ),
        ];

        assert_eq!(
            selected_capture_interfaces(&adapters),
            vec![InterfaceSelector::Index(2)]
        );
    }

    #[tokio::test]
    async fn selected_capture_decodes_real_frame_into_the_dhcp_cache_and_joins() {
        let owner = DhcpIntegration::new(None, None);
        let manager = owner.manager();
        let backend = Arc::new(FixtureCaptureBackend::new(&[1, 2, 3]));
        let adapters = vec![
            adapter(
                "fixture1",
                Some(1),
                false,
                false,
                &[Ipv4Addr::new(10, 0, 0, 1)],
            ),
            adapter(
                "fixture2",
                Some(2),
                true,
                true,
                &[Ipv4Addr::new(192, 168, 1, 2)],
            ),
            adapter(
                "fixture3",
                Some(3),
                true,
                false,
                &[Ipv4Addr::new(8, 8, 4, 4)],
            ),
        ];
        let providers = DhcpProviders::start_with(
            manager.clone(),
            adapters,
            Arc::clone(&backend),
            None::<Scanner<FakeNetworkProvider, FakePtrResolver>>,
            SCAN_INTERVAL,
        )
        .await
        .unwrap();

        assert_eq!(*backend.opened.lock().unwrap(), vec![2]);
        assert_eq!(backend.enumerate_calls.load(AtomicOrdering::Relaxed), 1);
        let weak = providers.capture_handles[0].weak();
        assert!(weak.is_running());
        backend.senders[&2].send(request_frame()).unwrap();

        let service_info = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = manager.service_info_snapshot().unwrap();
                if !snapshot.is_empty() {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("capture observation reached the DHCP cache");
        assert_eq!(
            service_info,
            vec![DhcpServiceInfo {
                ip: "192.168.1.45".to_owned(),
                hostname: "fixture-host".to_owned(),
                mac: "02aabbccddee".to_owned(),
            }]
        );

        providers.shutdown().await.unwrap();
        assert!(!weak.is_running());
        assert!(!weak.request_stop());
        assert!(backend.senders[&2].send(request_frame()).is_err());
        assert_eq!(manager.service_info_snapshot().unwrap(), service_info);
    }

    #[tokio::test]
    async fn empty_selection_never_enumerates_or_opens_a_capture_interface() {
        let owner = DhcpIntegration::new(None, None);
        let backend = Arc::new(FixtureCaptureBackend::new(&[1]));
        let providers = DhcpProviders::start_with(
            owner.manager(),
            vec![adapter(
                "global",
                Some(1),
                true,
                false,
                &[Ipv4Addr::new(1, 1, 1, 1)],
            )],
            Arc::clone(&backend),
            None::<Scanner<FakeNetworkProvider, FakePtrResolver>>,
            SCAN_INTERVAL,
        )
        .await
        .unwrap();

        assert_eq!(backend.enumerate_calls.load(AtomicOrdering::Relaxed), 0);
        assert!(backend.opened.lock().unwrap().is_empty());
        providers.shutdown().await.unwrap();
        assert_eq!(backend.enumerate_calls.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn scan_schedule_starts_immediately_runs_hourly_ticks_and_joins() {
        let owner = DhcpIntegration::new(None, None);
        let network = Arc::new(FakeNetworkProvider::default());
        let scanner = fake_scanner(Arc::clone(&network));
        let providers = DhcpProviders::start_with(
            owner.manager(),
            Vec::new(),
            Arc::new(FixtureCaptureBackend::new(&[])),
            Some(scanner),
            Duration::from_millis(20),
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_millis(150), async {
            while network.snapshot_calls.load(AtomicOrdering::Relaxed) < 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("immediate and periodic scans ran");

        providers.shutdown().await.unwrap();
        let calls_after_join = network.snapshot_calls.load(AtomicOrdering::Relaxed);
        tokio::time::sleep(Duration::from_millis(35)).await;
        assert_eq!(
            network.snapshot_calls.load(AtomicOrdering::Relaxed),
            calls_after_join
        );
        assert_eq!(SCAN_INTERVAL, Duration::from_secs(60 * 60));
    }

    #[tokio::test]
    async fn long_scan_skips_missed_ticks_instead_of_starting_a_catch_up_scan() {
        let owner = DhcpIntegration::new(None, None);
        let network = Arc::new(FakeNetworkProvider {
            snapshot_calls: AtomicUsize::new(0),
            snapshot_delay: Duration::from_millis(120),
            gate: None,
        });
        let scanner = fake_scanner(Arc::clone(&network));
        let providers = DhcpProviders::start_with(
            owner.manager(),
            Vec::new(),
            Arc::new(FixtureCaptureBackend::new(&[])),
            Some(scanner),
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(125)).await;
        assert_eq!(network.snapshot_calls.load(AtomicOrdering::Relaxed), 1);
        tokio::time::timeout(Duration::from_millis(100), async {
            while network.snapshot_calls.load(AtomicOrdering::Relaxed) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the next aligned tick started after the long scan");
        providers.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scanner_reports_overlap_and_cancels_its_blocked_provider() {
        let gate = Arc::new(ScanGate::default());
        let network = Arc::new(FakeNetworkProvider {
            snapshot_calls: AtomicUsize::new(0),
            snapshot_delay: Duration::ZERO,
            gate: Some(Arc::clone(&gate)),
        });
        let scanner = Arc::new(fake_scanner(network));
        let (stop, mut first_cancel) = watch::channel(false);
        let first_scanner = Arc::clone(&scanner);
        let first = tokio::spawn(async move { first_scanner.scan(&mut first_cancel).await });
        gate.started.notified().await;

        let (_other_stop, mut other_cancel) = watch::channel(false);
        let overlap = scanner.scan(&mut other_cancel).await.unwrap();
        assert_eq!(overlap.status, ScanStatus::SkippedOverlap);

        stop.send_replace(true);
        let cancelled = tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("cancelled scan joined")
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.status, ScanStatus::Cancelled);
    }

    fn fake_scanner(
        network: Arc<FakeNetworkProvider>,
    ) -> Scanner<FakeNetworkProvider, FakePtrResolver> {
        Scanner::new(
            network,
            Arc::new(FakePtrResolver),
            ScanConfig {
                fallback_prefix: 30,
                ..ScanConfig::default()
            },
        )
    }

    #[derive(Default)]
    struct ScanGate {
        started: Notify,
        released: Notify,
    }

    #[derive(Default)]
    struct FakeNetworkProvider {
        snapshot_calls: AtomicUsize,
        snapshot_delay: Duration,
        gate: Option<Arc<ScanGate>>,
    }

    impl NetworkProvider for FakeNetworkProvider {
        async fn network_snapshot(
            &self,
            selected_local_ip: Option<Ipv4Addr>,
        ) -> std::result::Result<NetworkSnapshot, ScanError> {
            self.snapshot_calls.fetch_add(1, AtomicOrdering::Relaxed);
            if let Some(gate) = &self.gate {
                gate.started.notify_one();
                gate.released.notified().await;
            }
            if !self.snapshot_delay.is_zero() {
                tokio::time::sleep(self.snapshot_delay).await;
            }
            Ok(NetworkSnapshot {
                local_ip: selected_local_ip.unwrap_or(Ipv4Addr::new(192, 168, 10, 1)),
                interface_prefix: Some(30),
                default_router: None,
                nameservers: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 10, 1))],
            })
        }

        async fn read_neighbors<'a>(
            &'a self,
            _ips: &'a [Ipv4Addr],
        ) -> std::result::Result<Vec<NeighborEntry>, ScanError> {
            Ok(Vec::new())
        }

        async fn probe_neighbors<'a>(
            &'a self,
            _ips: &'a [Ipv4Addr],
        ) -> std::result::Result<(), ScanError> {
            Ok(())
        }
    }

    struct FakePtrResolver;

    impl PtrResolver for FakePtrResolver {
        async fn lookup_ptr(
            &self,
            _nameserver: IpAddr,
            _ip: Ipv4Addr,
            _timeout: Duration,
        ) -> Option<String> {
            None
        }
    }

    fn request_frame() -> Vec<u8> {
        let mut options = vec![53, 1, 3, 50, 4, 192, 168, 1, 45];
        let hostname = b"fixture-host";
        options.extend_from_slice(&[12, hostname.len() as u8]);
        options.extend_from_slice(hostname);
        options.push(255);

        let mut bootp = vec![0; 240];
        bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bootp.extend_from_slice(&options);

        let udp_len = 8 + bootp.len();
        let mut udp = Vec::with_capacity(udp_len);
        udp.extend_from_slice(&68_u16.to_be_bytes());
        udp.extend_from_slice(&67_u16.to_be_bytes());
        udp.extend_from_slice(&(udp_len as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(&bootp);

        let ip_len = 20 + udp.len();
        let mut ip = vec![0; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&[0, 0, 0, 0]);
        ip[16..20].copy_from_slice(&[255, 255, 255, 255]);
        ip.extend_from_slice(&udp);

        let mut ethernet = vec![0xff; 6];
        ethernet.extend_from_slice(&[0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
        ethernet.extend_from_slice(&0x0800_u16.to_be_bytes());
        ethernet.extend_from_slice(&ip);
        ethernet
    }
}
