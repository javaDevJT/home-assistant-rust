use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    USB_SOURCE,
    debounce::{Debouncer, TimerScheduler},
    matcher::{USBMatcher, matching_usb_matchers},
    models::{
        PortDevice, SerialPortInfo, USBDevice, UsbServiceInfo, usb_serial_device_from_port,
        usb_service_info_from_device,
    },
    paths::{PathResolver, find_moved_serial_port, usb_device_from_path},
};

pub const MANUAL_SCAN_COOLDOWN: Duration = Duration::from_secs(10);
pub const ADD_REMOVE_SCAN_COOLDOWN: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbError {
    message: String,
}

impl UsbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for UsbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UsbError {}

/// Synchronous provider seam corresponding to `serialx.list_serial_ports()`.
/// Platform enumeration and hotplug handling are owned by the embedding app.
pub trait SerialPortProvider: Send + Sync {
    fn list_serial_ports(&self) -> Result<Vec<SerialPortInfo>, UsbError>;
}

impl<F> SerialPortProvider for F
where
    F: Fn() -> Result<Vec<SerialPortInfo>, UsbError> + Send + Sync,
{
    fn list_serial_ports(&self) -> Result<Vec<SerialPortInfo>, UsbError> {
        self()
    }
}

/// The pinned provider's placeholder-serial-number rule is injected rather
/// than guessed by this crate.
pub trait SerialNumberPolicy: Send + Sync {
    fn is_non_unique_serial(&self, serial_number: Option<&str>) -> bool;
}

/// External hotplug watcher seam. The integration strongly owns the watcher
/// after a successful start and joins it during stop.
pub trait UsbHotplugWatcher: Send + Sync {
    fn start(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), WatcherStartError>;
    fn stop_and_join(&self) -> Result<(), UsbError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WatcherStartError {
    InotifyNotAvailable(String),
    Other(UsbError),
}

impl<F> SerialNumberPolicy for F
where
    F: for<'a> Fn(Option<&'a str>) -> bool + Send + Sync,
{
    fn is_non_unique_serial(&self, serial_number: Option<&str>) -> bool {
        self(serial_number)
    }
}

pub type ScanRequestCallback = Arc<dyn Fn() -> Result<(), UsbError> + Send + Sync>;
pub type PortEventCallback =
    Arc<dyn Fn(&BTreeSet<USBDevice>, &BTreeSet<USBDevice>) -> Result<(), UsbError> + Send + Sync>;
pub type SerialPortScanner = Arc<dyn Fn() -> Result<Vec<PortDevice>, UsbError> + Send + Sync>;
pub type UsbDiscoveryCallback = Arc<dyn Fn(&DiscoveryAction) -> Result<(), UsbError> + Send + Sync>;
pub type ScanErrorCallback = Arc<dyn Fn(&UsbError) + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebounceRequest {
    /// An immediate debouncer executed its function in the current call.
    Ran,
    /// One trailing call is queued at the end of the current cooldown.
    Queued,
    /// The owner is stopped or the hotplug observer suppresses manual scans.
    Ignored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPlatform {
    Other,
    MacOs,
}

impl HostPlatform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
}

pub struct UsbOptions {
    pub matchers: Vec<USBMatcher>,
    pub platform: HostPlatform,
}

impl UsbOptions {
    pub fn new(matchers: Vec<USBMatcher>) -> Self {
        Self {
            matchers,
            platform: HostPlatform::current(),
        }
    }

    pub fn with_platform(mut self, platform: HostPlatform) -> Self {
        self.platform = platform;
        self
    }

    pub fn with_custom_matchers(custom: &[USBMatcher]) -> Self {
        Self {
            matchers: crate::matcher::merge_usb_matchers(
                &crate::generated::pinned_usb_matchers(),
                custom,
            ),
            platform: HostPlatform::current(),
        }
    }
}

impl Default for UsbOptions {
    fn default() -> Self {
        Self {
            matchers: crate::generated::pinned_usb_matchers(),
            platform: HostPlatform::current(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryContext {
    pub source: String,
}

/// USB create request for the shared discovery adapter. It intentionally has
/// no discovery-key field: pinned Home Assistant USB flows persist no key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryCreate {
    pub domain: String,
    pub source: String,
    pub context: DiscoveryContext,
    pub data: UsbServiceInfo,
}

/// A removal contains the exact service info and only the most-specific
/// matching domains. Flow protection policy belongs to the app adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryDismiss {
    pub data: UsbServiceInfo,
    pub matching_domains: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryAction {
    Create(DiscoveryCreate),
    Dismiss(DiscoveryDismiss),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannerFailure {
    pub scanner_id: u64,
    pub error: UsbError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackFailure {
    pub callback_id: u64,
    pub error: UsbError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialPortScan {
    /// Ports are in first-seen path order. A later provider/scanner record at
    /// the same exact `device` path replaces the existing value in place.
    pub ports: Vec<PortDevice>,
    pub scanner_failures: Vec<ScannerFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanReport {
    pub serial_ports: SerialPortScan,
    pub usb_devices: BTreeSet<USBDevice>,
    pub added_devices: BTreeSet<USBDevice>,
    pub removed_devices: BTreeSet<USBDevice>,
    /// Removal actions precede addition actions; matcher order is retained.
    pub actions: Vec<DiscoveryAction>,
    /// Port-event callback errors are isolated, like the source implementation.
    pub callback_failures: Vec<CallbackFailure>,
    pub initial_scan_completed: bool,
}

/// Strong owner for USB discovery state, callbacks, debounce tasks, and scans.
/// Drop or [`stop`](Self::stop) cancels pending work and waits for an active
/// provider/scanner pass to return.
pub struct UsbIntegration {
    inner: Arc<Inner>,
}

/// Cloneable weak entry point. It never owns the provider, callbacks, or timer
/// tasks and fails cleanly after the integration owner stops or is dropped.
#[derive(Clone)]
pub struct UsbManager {
    inner: Weak<Inner>,
}

struct Inner {
    active: AtomicBool,
    stop_gate: Mutex<()>,
    diagnostic_gate: Mutex<()>,
    observer_active: AtomicBool,
    provider: Arc<dyn SerialPortProvider>,
    matchers: Vec<USBMatcher>,
    platform: HostPlatform,
    serial_number_policy: Arc<dyn SerialNumberPolicy>,
    requested_gate: Mutex<()>,
    scan_gate: Mutex<()>,
    last_processed_devices: Mutex<BTreeSet<USBDevice>>,
    callbacks: Mutex<CallbackRegistry>,
    scanners: Mutex<ScannerRegistry>,
    next_id: Mutex<u64>,
    request_debouncer: Mutex<Option<Arc<Debouncer>>>,
    add_remove_debouncer: Mutex<Option<Arc<Debouncer>>>,
    watcher: Mutex<Option<Arc<dyn UsbHotplugWatcher>>>,
    scan_errors: Mutex<Vec<UsbError>>,
}

struct CallbackRegistry {
    initial_scan_done: bool,
    scan_requests: Vec<(u64, ScanRequestCallback)>,
    initial_scan: Vec<(u64, ScanRequestCallback)>,
    port_events: Vec<(u64, PortEventCallback)>,
    discovery: Vec<(u64, UsbDiscoveryCallback)>,
    scan_errors: Vec<(u64, ScanErrorCallback)>,
}

#[derive(Default)]
struct ScannerRegistry {
    scanners: Vec<(u64, SerialPortScanner)>,
}

#[derive(Clone, Copy)]
enum CallbackKind {
    ScanRequest,
    InitialScan,
    PortEvent,
    Discovery,
    ScanError,
}

/// Exact-registration RAII guard. Dropping it unregisters only its callback ID.
pub struct UsbCallbackGuard {
    inner: Weak<Inner>,
    id: Option<u64>,
    kind: CallbackKind,
}

/// Exact-registration RAII scanner guard.
pub struct UsbScannerGuard {
    inner: Weak<Inner>,
    id: u64,
}

impl UsbIntegration {
    pub fn new(
        provider: Arc<dyn SerialPortProvider>,
        scheduler: Arc<dyn TimerScheduler>,
        options: UsbOptions,
        serial_number_policy: Arc<dyn SerialNumberPolicy>,
    ) -> Self {
        let inner = Arc::new(Inner {
            active: AtomicBool::new(true),
            stop_gate: Mutex::new(()),
            diagnostic_gate: Mutex::new(()),
            observer_active: AtomicBool::new(false),
            provider,
            matchers: options.matchers,
            platform: options.platform,
            serial_number_policy,
            requested_gate: Mutex::new(()),
            scan_gate: Mutex::new(()),
            last_processed_devices: Mutex::new(BTreeSet::new()),
            callbacks: Mutex::new(CallbackRegistry {
                initial_scan_done: false,
                scan_requests: Vec::new(),
                initial_scan: Vec::new(),
                port_events: Vec::new(),
                discovery: Vec::new(),
                scan_errors: Vec::new(),
            }),
            scanners: Mutex::new(ScannerRegistry::default()),
            next_id: Mutex::new(1),
            request_debouncer: Mutex::new(None),
            add_remove_debouncer: Mutex::new(None),
            watcher: Mutex::new(None),
            scan_errors: Mutex::new(Vec::new()),
        });

        let weak = Arc::downgrade(&inner);
        let job: Arc<dyn Fn() -> Result<(), UsbError> + Send + Sync> = Arc::new(move || {
            let inner = weak
                .upgrade()
                .ok_or_else(|| UsbError::new("USB integration is stopped"))?;
            inner.run_requested_scan().map(|_| ())
        });
        let weak = Arc::downgrade(&inner);
        let on_error: Arc<dyn Fn(UsbError) + Send + Sync> = Arc::new(move |error| {
            if let Some(inner) = weak.upgrade() {
                inner.notify_scan_error(&error);
            }
        });
        let request_debouncer = Debouncer::new(
            Arc::clone(&scheduler),
            MANUAL_SCAN_COOLDOWN,
            true,
            Arc::clone(&job),
            Arc::clone(&on_error),
        );
        let add_remove_debouncer =
            Debouncer::new(scheduler, ADD_REMOVE_SCAN_COOLDOWN, false, job, on_error);
        *lock(&inner.request_debouncer) = Some(request_debouncer);
        *lock(&inner.add_remove_debouncer) = Some(add_remove_debouncer);
        Self { inner }
    }

    pub fn manager(&self) -> UsbManager {
        UsbManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn stop(&self) -> Result<(), UsbError> {
        self.inner.stop()
    }
}

impl Drop for UsbIntegration {
    fn drop(&mut self) {
        let _ = self.inner.stop();
    }
}

impl UsbManager {
    fn inner(&self) -> Result<Arc<Inner>, UsbError> {
        self.inner
            .upgrade()
            .filter(|inner| inner.active.load(Ordering::Acquire))
            .ok_or_else(|| UsbError::new("USB integration is stopped"))
    }

    pub fn set_observer_active(&self, active: bool) -> Result<(), UsbError> {
        self.inner()?
            .observer_active
            .store(active, Ordering::Release);
        Ok(())
    }

    /// Start an externally supplied watcher. Only the explicit inotify
    /// unavailable result is distinguishable for the caller's polling fallback.
    pub fn start_watcher(
        &self,
        watcher: Arc<dyn UsbHotplugWatcher>,
    ) -> Result<(), WatcherStartError> {
        let inner = self.inner().map_err(WatcherStartError::Other)?;
        let mut owned_watcher = lock(&inner.watcher);
        inner.ensure_active().map_err(WatcherStartError::Other)?;
        if owned_watcher.is_some() {
            return Err(WatcherStartError::Other(UsbError::new(
                "USB hotplug watcher is already started",
            )));
        }
        let weak_manager = self.clone();
        watcher.start(Arc::new(move || {
            let _ = weak_manager.notify_serial_ports_changed();
        }))?;
        inner.observer_active.store(true, Ordering::Release);
        *owned_watcher = Some(watcher);
        Ok(())
    }

    /// Full serial-port listing scan. Both USB and non-USB serial records are
    /// returned; only USB records enter the discovery snapshot.
    pub fn scan_serial_ports(&self) -> Result<SerialPortScan, UsbError> {
        let inner = self.inner()?;
        let _scan = lock(&inner.scan_gate);
        inner.ensure_active()?;
        inner.collect_ports()
    }

    /// A direct full scan used for Home Assistant's started event and polling.
    /// This intentionally skips scan-request callbacks and manual cooldown.
    pub fn scan_now(&self) -> Result<ScanReport, UsbError> {
        self.inner()?.scan_now()
    }

    /// Request a manual scan. An active hotplug observer suppresses this route;
    /// otherwise the first call is immediate with one queued trailing run per
    /// ten-second cooldown, matching Home Assistant's immediate Debouncer.
    pub fn request_scan(&self) -> Result<DebounceRequest, UsbError> {
        let inner = self.inner()?;
        if inner.observer_active.load(Ordering::Acquire) {
            return Ok(DebounceRequest::Ignored);
        }
        let debouncer = lock(&inner.request_debouncer)
            .as_ref()
            .cloned()
            .ok_or_else(|| UsbError::new("USB manual-scan debouncer is stopped"))?;
        debouncer.request()
    }

    /// Schedule a delayed scan after scanner-contributed ports change. This
    /// path remains available while a hotplug observer is active.
    pub fn notify_serial_ports_changed(&self) -> Result<DebounceRequest, UsbError> {
        let inner = self.inner()?;
        let debouncer = lock(&inner.add_remove_debouncer)
            .as_ref()
            .cloned()
            .ok_or_else(|| UsbError::new("USB change debouncer is stopped"))?;
        debouncer.request()
    }

    pub fn initial_scan_done(&self) -> Result<bool, UsbError> {
        let inner = self.inner()?;
        let result = lock(&inner.callbacks).initial_scan_done;
        Ok(result)
    }

    pub fn last_processed_devices(&self) -> Result<BTreeSet<USBDevice>, UsbError> {
        let inner = self.inner()?;
        let result = lock(&inner.last_processed_devices).clone();
        Ok(result)
    }

    /// Return only the most-specific matching integration domains, preserving
    /// the generated-core-then-custom matcher order and tied matches.
    pub fn matching_integrations(&self, device: &USBDevice) -> Result<Vec<String>, UsbError> {
        let inner = self.inner()?;
        Ok(matching_usb_matchers(device, &inner.matchers)
            .into_iter()
            .map(|matcher| matcher.domain.clone())
            .collect())
    }

    pub fn take_scan_errors(&self) -> Result<Vec<UsbError>, UsbError> {
        let inner = self.inner()?;
        let result = std::mem::take(&mut *lock(&inner.scan_errors));
        Ok(result)
    }

    pub fn find_moved_serial_port(&self, device_path: &str) -> Result<Option<String>, UsbError> {
        let inner = self.inner()?;
        let scanned: Vec<_> = lock(&inner.last_processed_devices)
            .iter()
            .cloned()
            .collect();
        let policy = Arc::clone(&inner.serial_number_policy);
        Ok(find_moved_serial_port(
            device_path,
            &scanned,
            move |serial| policy.is_non_unique_serial(serial),
        ))
    }

    /// Fresh provider lookup matching the source helper's realpath comparison.
    pub fn usb_device_from_path(
        &self,
        device_path: &str,
        resolver: &impl PathResolver,
    ) -> Result<Option<USBDevice>, UsbError> {
        let inner = self.inner()?;
        let _scan = lock(&inner.scan_gate);
        let ports = inner.collect_provider_ports()?;
        Ok(usb_device_from_path(device_path, &ports, resolver))
    }

    pub fn register_scan_request_callback<F>(
        &self,
        callback: F,
    ) -> Result<UsbCallbackGuard, UsbError>
    where
        F: Fn() -> Result<(), UsbError> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_id();
        lock(&inner.callbacks)
            .scan_requests
            .push((id, Arc::new(callback)));
        Ok(UsbCallbackGuard::new(&inner, id, CallbackKind::ScanRequest))
    }

    pub fn register_initial_scan_callback<F>(
        &self,
        callback: F,
    ) -> Result<UsbCallbackGuard, UsbError>
    where
        F: Fn() -> Result<(), UsbError> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let callback: ScanRequestCallback = Arc::new(callback);
        let mut invoke_now = false;
        let id = {
            let mut callbacks = lock(&inner.callbacks);
            if callbacks.initial_scan_done {
                invoke_now = true;
                None
            } else {
                let id = inner.next_id();
                callbacks.initial_scan.push((id, callback.clone()));
                Some(id)
            }
        };
        if invoke_now {
            callback()?;
        }
        Ok(UsbCallbackGuard::new(
            &inner,
            id.unwrap_or(0),
            CallbackKind::InitialScan,
        ))
    }

    pub fn register_port_event_callback<F>(&self, callback: F) -> Result<UsbCallbackGuard, UsbError>
    where
        F: Fn(&BTreeSet<USBDevice>, &BTreeSet<USBDevice>) -> Result<(), UsbError>
            + Send
            + Sync
            + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_id();
        lock(&inner.callbacks)
            .port_events
            .push((id, Arc::new(callback)));
        Ok(UsbCallbackGuard::new(&inner, id, CallbackKind::PortEvent))
    }

    pub fn register_discovery_callback<F>(&self, callback: F) -> Result<UsbCallbackGuard, UsbError>
    where
        F: Fn(&DiscoveryAction) -> Result<(), UsbError> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_id();
        lock(&inner.callbacks)
            .discovery
            .push((id, Arc::new(callback)));
        Ok(UsbCallbackGuard::new(&inner, id, CallbackKind::Discovery))
    }

    pub fn register_scan_error_callback<F>(&self, callback: F) -> Result<UsbCallbackGuard, UsbError>
    where
        F: Fn(&UsbError) + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_id();
        lock(&inner.callbacks)
            .scan_errors
            .push((id, Arc::new(callback)));
        Ok(UsbCallbackGuard::new(&inner, id, CallbackKind::ScanError))
    }

    pub fn register_serial_port_scanner<F>(&self, scanner: F) -> Result<UsbScannerGuard, UsbError>
    where
        F: Fn() -> Result<Vec<PortDevice>, UsbError> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_id();
        lock(&inner.scanners).scanners.push((id, Arc::new(scanner)));
        Ok(UsbScannerGuard {
            inner: Arc::downgrade(&inner),
            id,
        })
    }
}

impl Inner {
    fn next_id(&self) -> u64 {
        let mut next = lock(&self.next_id);
        let id = *next;
        *next = next.wrapping_add(1).max(1);
        id
    }

    fn ensure_active(&self) -> Result<(), UsbError> {
        self.active
            .load(Ordering::Acquire)
            .then_some(())
            .ok_or_else(|| UsbError::new("USB integration is stopped"))
    }

    fn stop(&self) -> Result<(), UsbError> {
        let _stop = lock(&self.stop_gate);
        if !self.active.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.observer_active.store(false, Ordering::Release);
        let watcher = { lock(&self.watcher).take() };
        let watcher_error = watcher.and_then(|watcher| watcher.stop_and_join().err());
        if let Some(debouncer) = lock(&self.request_debouncer).take() {
            debouncer.shutdown();
        }
        if let Some(debouncer) = lock(&self.add_remove_debouncer).take() {
            debouncer.shutdown();
        }
        // A requested scan begins with integration callbacks before acquiring
        // the provider gate. Both phases must finish before stop returns.
        let _requested = lock(&self.requested_gate);
        let _scan = lock(&self.scan_gate);
        let _diagnostics = lock(&self.diagnostic_gate);
        lock(&self.callbacks).clear();
        lock(&self.scanners).scanners.clear();
        watcher_error.map_or(Ok(()), Err)
    }

    fn collect_provider_ports(&self) -> Result<Vec<PortDevice>, UsbError> {
        self.ensure_active()?;
        let base = catch_unwind(AssertUnwindSafe(|| self.provider.list_serial_ports()))
            .map_err(|_| UsbError::new("USB base provider panicked"))??;
        self.ensure_active()?;
        Ok(base.into_iter().map(usb_serial_device_from_port).collect())
    }

    fn collect_ports(&self) -> Result<SerialPortScan, UsbError> {
        let base = self.collect_provider_ports()?;
        let mut ports = Vec::new();
        let mut positions = HashMap::new();
        for provider_port in base {
            merge_port(&mut ports, &mut positions, provider_port);
        }

        let scanners = lock(&self.scanners).scanners.clone();
        let mut scanner_failures = Vec::new();
        for (scanner_id, scanner) in scanners {
            self.ensure_active()?;
            let result = catch_unwind(AssertUnwindSafe(|| scanner()));
            match result {
                Ok(Ok(contributions)) => {
                    for port in contributions {
                        merge_port(&mut ports, &mut positions, port);
                    }
                }
                Ok(Err(error)) => scanner_failures.push(ScannerFailure { scanner_id, error }),
                Err(_) => scanner_failures.push(ScannerFailure {
                    scanner_id,
                    error: UsbError::new("serial-port scanner panicked"),
                }),
            }
        }
        self.ensure_active()?;
        Ok(SerialPortScan {
            ports,
            scanner_failures,
        })
    }

    fn run_requested_scan(&self) -> Result<ScanReport, UsbError> {
        let _requested = lock(&self.requested_gate);
        self.ensure_active()?;
        let callbacks: Vec<_> = lock(&self.callbacks)
            .scan_requests
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect();
        for callback in callbacks {
            callback()?;
        }
        let report = self.scan_now()?;
        // The debouncer returns only a unit result. Route isolated failures to
        // the host's diagnostic observer before that report is discarded.
        for failure in &report.serial_ports.scanner_failures {
            self.notify_scan_error(&UsbError::new(format!(
                "USB serial-port scanner {} failed: {}",
                failure.scanner_id, failure.error
            )));
        }
        for failure in &report.callback_failures {
            self.notify_scan_error(&UsbError::new(format!(
                "USB port-event callback {} failed: {}",
                failure.callback_id, failure.error
            )));
        }
        Ok(report)
    }

    fn scan_now(&self) -> Result<ScanReport, UsbError> {
        self.ensure_active()?;
        let _scan = lock(&self.scan_gate);
        self.ensure_active()?;
        let serial_ports = self.collect_ports()?;
        self.ensure_active()?;
        let usb_devices = self.filtered_usb_snapshot(&serial_ports.ports);
        let (added_devices, removed_devices) = {
            let mut last = lock(&self.last_processed_devices);
            let added: BTreeSet<_> = usb_devices.difference(&last).cloned().collect();
            let removed: BTreeSet<_> = last.difference(&usb_devices).cloned().collect();
            *last = usb_devices.clone();
            (added, removed)
        };

        let mut callback_failures = Vec::new();
        if !added_devices.is_empty() || !removed_devices.is_empty() {
            let callbacks: Vec<_> = lock(&self.callbacks)
                .port_events
                .iter()
                .map(|(id, callback)| (*id, callback.clone()))
                .collect();
            for (callback_id, callback) in callbacks {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    callback(&added_devices, &removed_devices)
                }))
                .unwrap_or_else(|_| Err(UsbError::new("USB port-event callback panicked")));
                if let Err(error) = result {
                    callback_failures.push(CallbackFailure { callback_id, error });
                }
            }
        }

        let mut actions = Vec::new();
        // Source processes removals before additions. BTreeSet ordering is an
        // explicit deterministic representation of the source's set results.
        for device in &removed_devices {
            let matches = matching_usb_matchers(device, &self.matchers);
            if matches.is_empty() {
                continue;
            }
            actions.push(DiscoveryAction::Dismiss(DiscoveryDismiss {
                data: usb_service_info_from_device(device),
                matching_domains: matches
                    .into_iter()
                    .map(|matcher| matcher.domain.clone())
                    .collect(),
            }));
        }
        for device in &added_devices {
            let matches = matching_usb_matchers(device, &self.matchers);
            if matches.is_empty() {
                continue;
            }
            let data = usb_service_info_from_device(device);
            for matcher in matches {
                actions.push(DiscoveryAction::Create(DiscoveryCreate {
                    domain: matcher.domain.clone(),
                    source: USB_SOURCE.to_owned(),
                    context: DiscoveryContext {
                        source: USB_SOURCE.to_owned(),
                    },
                    data: data.clone(),
                }));
            }
        }

        if !actions.is_empty() {
            let callbacks: Vec<_> = lock(&self.callbacks)
                .discovery
                .iter()
                .map(|(_, callback)| callback.clone())
                .collect();
            for action in &actions {
                for callback in &callbacks {
                    match catch_unwind(AssertUnwindSafe(|| callback(action))) {
                        Ok(result) => result?,
                        Err(_) => return Err(UsbError::new("USB discovery callback panicked")),
                    }
                }
            }
        }

        let initial_scan_completed = self.complete_initial_scan()?;
        Ok(ScanReport {
            serial_ports,
            usb_devices,
            added_devices,
            removed_devices,
            actions,
            callback_failures,
            initial_scan_completed,
        })
    }

    fn complete_initial_scan(&self) -> Result<bool, UsbError> {
        {
            let mut callbacks = lock(&self.callbacks);
            if callbacks.initial_scan_done {
                return Ok(false);
            }
            callbacks.initial_scan_done = true;
        }
        loop {
            let callback = lock(&self.callbacks).initial_scan.pop();
            let Some((_, callback)) = callback else {
                break;
            };
            // The source calls these in reverse registration order and does not
            // isolate exceptions. A failing callback leaves older callbacks
            // queued even though the initial-scan flag is already true.
            callback()?;
        }
        Ok(true)
    }

    fn filtered_usb_snapshot(&self, ports: &[PortDevice]) -> BTreeSet<USBDevice> {
        let devices: Vec<_> = ports
            .iter()
            .filter_map(|port| port.as_usb().cloned())
            .collect();
        if self.platform != HostPlatform::MacOs {
            return devices.into_iter().collect();
        }
        let silabs_serials: BTreeSet<_> = devices
            .iter()
            .filter(|device| device.device.starts_with("/dev/cu.SLAB_USBtoUART"))
            .map(|device| device.serial_number.clone())
            .collect();
        devices
            .into_iter()
            .filter(|device| {
                !silabs_serials.contains(&device.serial_number)
                    || device.device.starts_with("/dev/cu.SLAB_USBtoUART")
            })
            .collect()
    }

    fn notify_scan_error(&self, error: &UsbError) {
        let _dispatch = lock(&self.diagnostic_gate);
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        lock(&self.scan_errors).push(error.clone());
        let callbacks: Vec<_> = lock(&self.callbacks)
            .scan_errors
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect();
        for callback in callbacks {
            let _ = catch_unwind(AssertUnwindSafe(|| callback(error)));
        }
    }
}

impl CallbackRegistry {
    fn remove(&mut self, id: u64, kind: CallbackKind) {
        match kind {
            CallbackKind::ScanRequest => self.scan_requests.retain(|(entry_id, _)| *entry_id != id),
            CallbackKind::InitialScan => self.initial_scan.retain(|(entry_id, _)| *entry_id != id),
            CallbackKind::PortEvent => self.port_events.retain(|(entry_id, _)| *entry_id != id),
            CallbackKind::Discovery => self.discovery.retain(|(entry_id, _)| *entry_id != id),
            CallbackKind::ScanError => self.scan_errors.retain(|(entry_id, _)| *entry_id != id),
        }
    }

    fn clear(&mut self) {
        self.scan_requests.clear();
        self.initial_scan.clear();
        self.port_events.clear();
        self.discovery.clear();
        self.scan_errors.clear();
    }
}

impl UsbCallbackGuard {
    fn new(inner: &Arc<Inner>, id: u64, kind: CallbackKind) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            id: (id != 0).then_some(id),
            kind,
        }
    }
}

impl Drop for UsbCallbackGuard {
    fn drop(&mut self) {
        let (Some(inner), Some(id)) = (self.inner.upgrade(), self.id.take()) else {
            return;
        };
        if inner.active.load(Ordering::Acquire) {
            lock(&inner.callbacks).remove(id, self.kind);
        }
    }
}

impl Drop for UsbScannerGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.active.load(Ordering::Acquire) {
            lock(&inner.scanners)
                .scanners
                .retain(|(entry_id, _)| *entry_id != self.id);
        }
    }
}

fn merge_port(
    ports: &mut Vec<PortDevice>,
    positions: &mut HashMap<String, usize>,
    port: PortDevice,
) {
    let path = port.device().to_owned();
    if let Some(position) = positions.get(&path).copied() {
        ports[position] = port;
    } else {
        positions.insert(path, ports.len());
        ports.push(port);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
