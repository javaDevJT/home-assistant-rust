use crate::{DecodeError, WireObservation, decode_ethernet_frame};
use pnet_datalink::{self, Channel, Config, DataLinkReceiver};
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Select an interface by its OS-specific name or index.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum InterfaceSelector {
    Name(String),
    Index(u32),
}

impl From<&str> for InterfaceSelector {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}

impl From<String> for InterfaceSelector {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// A resolved interface identity used by capture backends.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CaptureInterface {
    pub name: String,
    pub index: u32,
}

/// Interface selection and the maximum time a frame source may block per read.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub interfaces: Vec<InterfaceSelector>,
    pub read_timeout: Duration,
}

impl CaptureConfig {
    pub fn new(interfaces: Vec<InterfaceSelector>) -> Self {
        Self {
            interfaces,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }

    pub fn with_read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// An injectable source of Ethernet frames.
///
/// Implementations must return within `timeout` when no frame is available.
/// This is what lets the owner set the stop flag and join every reader thread.
pub trait CaptureSource: Send + 'static {
    fn recv_frame(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;
}

/// Interface discovery and selected-interface opening boundary.
pub trait CaptureBackend: Send + Sync + 'static {
    fn interfaces(&self) -> io::Result<Vec<CaptureInterface>>;

    fn open_source(
        &self,
        interface: &CaptureInterface,
        read_timeout: Duration,
    ) -> io::Result<Box<dyn CaptureSource>>;
}

/// The production passive backend backed by `pnet_datalink`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PnetCaptureBackend;

impl CaptureBackend for PnetCaptureBackend {
    fn interfaces(&self) -> io::Result<Vec<CaptureInterface>> {
        Ok(pnet_datalink::interfaces()
            .into_iter()
            .map(|interface| CaptureInterface {
                name: interface.name,
                index: interface.index,
            })
            .collect())
    }

    fn open_source(
        &self,
        interface: &CaptureInterface,
        read_timeout: Duration,
    ) -> io::Result<Box<dyn CaptureSource>> {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (interface, read_timeout);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "pnet read timeouts are only verified here for Linux and macOS; capture refused",
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let selected = pnet_datalink::interfaces()
                .into_iter()
                .find(|candidate| {
                    candidate.name == interface.name && candidate.index == interface.index
                })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "selected interface {} (index {}) disappeared before capture opened",
                            interface.name, interface.index
                        ),
                    )
                })?;

            let config = Config {
                read_timeout: Some(read_timeout),
                // The pinned Scapy watcher explicitly disables promiscuous mode.
                promiscuous: false,
                ..Config::default()
            };
            match pnet_datalink::channel(&selected, config)? {
                Channel::Ethernet(_, receiver) => Ok(Box::new(PnetFrameSource { receiver })),
                _ => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "selected interface did not provide Ethernet frames",
                )),
            }
        }
    }
}

struct PnetFrameSource {
    receiver: Box<dyn DataLinkReceiver>,
}

impl CaptureSource for PnetFrameSource {
    fn recv_frame(&mut self, _timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        match self.receiver.next() {
            Ok(frame) => Ok(Some(frame.to_vec())),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

struct SharedState {
    stop_requested: AtomicBool,
    worker_count: usize,
    joined: AtomicBool,
}

/// A weak, non-owning way to request capture shutdown.
#[derive(Clone)]
pub struct WeakCaptureHandle {
    state: Weak<SharedState>,
}

impl WeakCaptureHandle {
    /// Request that all readers stop. The owning handle must still join them.
    pub fn request_stop(&self) -> bool {
        if let Some(state) = self.state.upgrade() {
            state.stop_requested.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            state.worker_count > 0 && !state.stop_requested.load(Ordering::Acquire)
        })
    }
}

/// Observable lifetime state of a capture handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureStatus {
    Idle,
    Running,
    Stopping,
    Stopped,
}

/// Strong owner of the capture workers. Dropping it stops and joins them.
pub struct CaptureHandle {
    state: Arc<SharedState>,
    workers: Vec<(CaptureInterface, JoinHandle<Result<(), CaptureWorkerError>>)>,
}

impl CaptureHandle {
    pub fn weak(&self) -> WeakCaptureHandle {
        WeakCaptureHandle {
            state: Arc::downgrade(&self.state),
        }
    }

    pub fn request_stop(&self) {
        self.state.stop_requested.store(true, Ordering::Release);
    }

    pub fn status(&self) -> CaptureStatus {
        if self.state.joined.load(Ordering::Acquire) {
            CaptureStatus::Stopped
        } else if self.state.stop_requested.load(Ordering::Acquire) {
            CaptureStatus::Stopping
        } else if self.state.worker_count == 0 {
            CaptureStatus::Idle
        } else {
            CaptureStatus::Running
        }
    }

    /// Stop readers and synchronously join every worker thread.
    pub fn shutdown(mut self) -> Result<(), CaptureShutdownError> {
        self.join_workers()
    }

    /// Stop readers and join them on Tokio's blocking pool.
    ///
    /// If no Tokio runtime is active, this performs the same join synchronously.
    pub async fn shutdown_async(mut self) -> Result<(), CaptureShutdownError> {
        self.state.stop_requested.store(true, Ordering::Release);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime
                .spawn_blocking(move || self.join_workers())
                .await
                .map_err(|error| CaptureShutdownError::AsyncJoin(error.to_string()))?,
            Err(_) => self.join_workers(),
        }
    }

    fn join_workers(&mut self) -> Result<(), CaptureShutdownError> {
        self.state.stop_requested.store(true, Ordering::Release);
        let mut first_error = None;
        for (interface, worker) in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(CaptureShutdownError::Worker(error));
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(CaptureShutdownError::WorkerPanicked(interface));
                    }
                }
            }
        }
        self.state.joined.store(true, Ordering::Release);
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.join_workers();
    }
}

/// Errors encountered before capture workers start.
#[derive(Debug)]
pub enum CaptureStartError {
    InvalidReadTimeout,
    InvalidSelector(InterfaceSelector),
    InterfaceEnumeration(io::Error),
    InterfaceNotFound(InterfaceSelector),
    Open {
        interface: CaptureInterface,
        source: io::Error,
    },
    ThreadSpawn {
        interface: CaptureInterface,
        source: io::Error,
    },
}

impl fmt::Display for CaptureStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReadTimeout => write!(
                f,
                "capture read timeout must be greater than zero and at most {} seconds",
                MAX_READ_TIMEOUT.as_secs()
            ),
            Self::InvalidSelector(selector) => write!(f, "invalid interface selector {selector:?}"),
            Self::InterfaceEnumeration(error) => {
                write!(f, "failed to enumerate interfaces: {error}")
            }
            Self::InterfaceNotFound(selector) => {
                write!(f, "selected interface {selector:?} was not found")
            }
            Self::Open { interface, source } => write!(
                f,
                "failed to open selected interface {} (index {}): {source}",
                interface.name, interface.index
            ),
            Self::ThreadSpawn { interface, source } => write!(
                f,
                "failed to start reader for selected interface {} (index {}): {source}",
                interface.name, interface.index
            ),
        }
    }
}

impl std::error::Error for CaptureStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InterfaceEnumeration(error)
            | Self::Open { source: error, .. }
            | Self::ThreadSpawn { source: error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Errors reported after capture workers have started.
#[derive(Debug)]
pub enum CaptureShutdownError {
    Worker(CaptureWorkerError),
    WorkerPanicked(CaptureInterface),
    AsyncJoin(String),
}

impl fmt::Display for CaptureShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worker(error) => write!(f, "capture worker failed: {error}"),
            Self::WorkerPanicked(interface) => write!(
                f,
                "capture worker for {} (index {}) panicked",
                interface.name, interface.index
            ),
            Self::AsyncJoin(error) => write!(f, "async capture join failed: {error}"),
        }
    }
}

impl std::error::Error for CaptureShutdownError {}

#[derive(Debug)]
pub struct CaptureWorkerError {
    pub interface: CaptureInterface,
    pub kind: CaptureWorkerErrorKind,
}

#[derive(Debug)]
pub enum CaptureWorkerErrorKind {
    Read(io::Error),
    CallbackPanicked,
}

impl fmt::Display for CaptureWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (index {}): ",
            self.interface.name, self.interface.index
        )?;
        match &self.kind {
            CaptureWorkerErrorKind::Read(error) => write!(f, "{error}"),
            CaptureWorkerErrorKind::CallbackPanicked => write!(f, "observation callback panicked"),
        }
    }
}

/// Start one reader only for each explicitly selected interface.
///
/// An empty interface list returns an idle owner without enumerating or opening
/// any interface. The callback runs on a reader thread and should return
/// promptly; shutdown joins it after the configured receive timeout.
pub fn start_capture<B, F>(
    backend: Arc<B>,
    config: CaptureConfig,
    callback: F,
) -> Result<CaptureHandle, CaptureStartError>
where
    B: CaptureBackend,
    F: Fn(WireObservation) + Send + Sync + 'static,
{
    if config.interfaces.is_empty() {
        return Ok(CaptureHandle {
            state: Arc::new(SharedState {
                stop_requested: AtomicBool::new(false),
                worker_count: 0,
                joined: AtomicBool::new(false),
            }),
            workers: Vec::new(),
        });
    }

    if config.read_timeout.is_zero() || config.read_timeout > MAX_READ_TIMEOUT {
        return Err(CaptureStartError::InvalidReadTimeout);
    }
    for selector in &config.interfaces {
        if matches!(selector, InterfaceSelector::Name(name) if name.is_empty())
            || matches!(selector, InterfaceSelector::Index(0))
        {
            return Err(CaptureStartError::InvalidSelector(selector.clone()));
        }
    }

    let available = backend
        .interfaces()
        .map_err(CaptureStartError::InterfaceEnumeration)?;
    let selected = resolve_interfaces(&config.interfaces, &available)?;

    // Open every selected source before starting any workers. If an interface
    // fails, previously opened sources are dropped and no read loop is left.
    let mut sources = Vec::with_capacity(selected.len());
    for interface in &selected {
        let source = backend
            .open_source(interface, config.read_timeout)
            .map_err(|source| CaptureStartError::Open {
                interface: interface.clone(),
                source,
            })?;
        sources.push((interface.clone(), source));
    }

    let state = Arc::new(SharedState {
        stop_requested: AtomicBool::new(false),
        worker_count: sources.len(),
        joined: AtomicBool::new(false),
    });
    let callback = Arc::new(callback);
    let mut workers = Vec::with_capacity(sources.len());

    for (interface, mut source) in sources {
        let worker_state = Arc::clone(&state);
        let worker_callback = Arc::clone(&callback);
        let worker_interface = interface.clone();
        let timeout = config.read_timeout;
        let spawn = thread::Builder::new()
            .name(format!("hass-dhcp-{}", interface.name))
            .spawn(move || {
                loop {
                    if worker_state.stop_requested.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    match source.recv_frame(timeout) {
                        Ok(Some(frame)) => match decode_ethernet_frame(&frame) {
                            Ok(Some(observation)) => {
                                if worker_state.stop_requested.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    worker_callback(observation)
                                }))
                                .is_err()
                                {
                                    worker_state.stop_requested.store(true, Ordering::Release);
                                    return Err(CaptureWorkerError {
                                        interface: worker_interface.clone(),
                                        kind: CaptureWorkerErrorKind::CallbackPanicked,
                                    });
                                }
                            }
                            Ok(None)
                            | Err(DecodeError::Truncated(_))
                            | Err(DecodeError::Malformed(_)) => {
                                // The upstream handler ignores nonrequests. Bad
                                // untrusted frames are discarded without ending capture.
                            }
                        },
                        Ok(None) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut
                                    | io::ErrorKind::WouldBlock
                                    | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => {
                            worker_state.stop_requested.store(true, Ordering::Release);
                            return Err(CaptureWorkerError {
                                interface: worker_interface.clone(),
                                kind: CaptureWorkerErrorKind::Read(error),
                            });
                        }
                    }
                }
            });

        match spawn {
            Ok(worker) => workers.push((interface, worker)),
            Err(source) => {
                state.stop_requested.store(true, Ordering::Release);
                for (_, worker) in workers.drain(..) {
                    let _ = worker.join();
                }
                return Err(CaptureStartError::ThreadSpawn { interface, source });
            }
        }
    }

    Ok(CaptureHandle { state, workers })
}

fn resolve_interfaces(
    selectors: &[InterfaceSelector],
    available: &[CaptureInterface],
) -> Result<Vec<CaptureInterface>, CaptureStartError> {
    let mut selected = Vec::new();
    let mut selected_indexes = HashSet::new();
    for selector in selectors {
        let interface = available
            .iter()
            .find(|interface| match selector {
                InterfaceSelector::Name(name) => &interface.name == name,
                InterfaceSelector::Index(index) => interface.index == *index,
            })
            .cloned()
            .ok_or_else(|| CaptureStartError::InterfaceNotFound(selector.clone()))?;
        if selected_indexes.insert(interface.index) {
            selected.push(interface);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::Instant;

    struct FixtureBackend {
        available: Vec<CaptureInterface>,
        sources: Mutex<HashMap<u32, Receiver<Vec<u8>>>>,
        opened: Mutex<Vec<u32>>,
        list_calls: AtomicUsize,
        fail_open: Option<(u32, io::ErrorKind)>,
    }

    impl FixtureBackend {
        fn new(available: Vec<CaptureInterface>) -> (Self, HashMap<u32, Sender<Vec<u8>>>) {
            let mut sources = HashMap::new();
            let mut senders = HashMap::new();
            for interface in &available {
                let (sender, receiver) = mpsc::channel();
                sources.insert(interface.index, receiver);
                senders.insert(interface.index, sender);
            }
            (
                Self {
                    available,
                    sources: Mutex::new(sources),
                    opened: Mutex::new(Vec::new()),
                    list_calls: AtomicUsize::new(0),
                    fail_open: None,
                },
                senders,
            )
        }

        fn fail_open(mut self, interface_index: u32, kind: io::ErrorKind) -> Self {
            self.fail_open = Some((interface_index, kind));
            self
        }
    }

    impl CaptureBackend for FixtureBackend {
        fn interfaces(&self) -> io::Result<Vec<CaptureInterface>> {
            self.list_calls.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(self.available.clone())
        }

        fn open_source(
            &self,
            interface: &CaptureInterface,
            _read_timeout: Duration,
        ) -> io::Result<Box<dyn CaptureSource>> {
            self.opened.lock().unwrap().push(interface.index);
            if let Some((index, kind)) = self.fail_open
                && index == interface.index
            {
                return Err(io::Error::new(kind, "fixture open failure"));
            }
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

    fn available_interfaces() -> Vec<CaptureInterface> {
        vec![
            CaptureInterface {
                name: "en0".to_owned(),
                index: 1,
            },
            CaptureInterface {
                name: "en1".to_owned(),
                index: 2,
            },
        ]
    }

    fn frame() -> Vec<u8> {
        crate::decode::test_support::request_frame()
    }

    #[test]
    fn empty_selection_is_idle_and_never_enumerates_interfaces() {
        let (backend, _) = FixtureBackend::new(available_interfaces());
        let backend = Arc::new(backend);
        let handle = start_capture(Arc::clone(&backend), CaptureConfig::default(), |_| {
            panic!("idle capture cannot report a packet")
        })
        .unwrap();
        assert_eq!(handle.status(), CaptureStatus::Idle);
        assert!(!handle.weak().is_running());
        assert_eq!(backend.list_calls.load(AtomicOrdering::Relaxed), 0);
        assert!(backend.opened.lock().unwrap().is_empty());
        handle.shutdown().unwrap();
    }

    #[test]
    fn only_explicit_interfaces_are_opened_and_duplicate_selection_is_deduplicated() {
        let (backend, senders) = FixtureBackend::new(available_interfaces());
        let backend = Arc::new(backend);
        let (observed_tx, observed_rx) = mpsc::channel();
        let handle = start_capture(
            Arc::clone(&backend),
            CaptureConfig::new(vec![
                InterfaceSelector::Index(2),
                InterfaceSelector::Name("en1".to_owned()),
            ]),
            move |observation| observed_tx.send(observation).unwrap(),
        )
        .unwrap();
        assert_eq!(*backend.opened.lock().unwrap(), vec![2]);

        senders[&2].send(frame()).unwrap();
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(1)).unwrap().ip,
            "192.0.2.19"
        );
        let started = Instant::now();
        handle.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unknown_interfaces_and_permission_failures_are_returned() {
        let (backend, _) = FixtureBackend::new(available_interfaces());
        let error = match start_capture(
            Arc::new(backend),
            CaptureConfig::new(vec![InterfaceSelector::Name("en9".to_owned())]),
            |_| {},
        ) {
            Err(error) => error,
            Ok(handle) => {
                handle.shutdown().unwrap();
                panic!("unknown interface unexpectedly started capture")
            }
        };
        assert!(matches!(error, CaptureStartError::InterfaceNotFound(_)));

        let (backend, _) = FixtureBackend::new(available_interfaces());
        let backend = backend.fail_open(2, io::ErrorKind::PermissionDenied);
        let error = match start_capture(
            Arc::new(backend),
            CaptureConfig::new(vec![InterfaceSelector::Index(2)]),
            |_| {},
        ) {
            Err(error) => error,
            Ok(handle) => {
                handle.shutdown().unwrap();
                panic!("permission failure unexpectedly started capture")
            }
        };
        assert!(matches!(
            error,
            CaptureStartError::Open { source, .. }
                if source.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn weak_stop_and_sync_shutdown_join_the_reader() {
        let (backend, senders) = FixtureBackend::new(available_interfaces());
        let (observed_tx, observed_rx) = mpsc::channel();
        let handle = start_capture(
            Arc::new(backend),
            CaptureConfig::new(vec![InterfaceSelector::Index(1)])
                .with_read_timeout(Duration::from_millis(25)),
            move |observation| observed_tx.send(observation).unwrap(),
        )
        .unwrap();
        let weak = handle.weak();
        senders[&1].send(frame()).unwrap();
        observed_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(weak.request_stop());
        let started = Instant::now();
        handle.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!weak.is_running());
    }

    #[test]
    fn async_shutdown_joins_reader_threads() {
        let (backend, _) = FixtureBackend::new(available_interfaces());
        let handle = start_capture(
            Arc::new(backend),
            CaptureConfig::new(vec![InterfaceSelector::Index(1)])
                .with_read_timeout(Duration::from_millis(25)),
            |_| {},
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(handle.shutdown_async()).unwrap();
    }
}
