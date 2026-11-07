//! Owned adapter from the native Zeroconf SDK to the vendored mDNS wire daemon.
//!
//! `MdnsWireOwner` owns the one daemon thread and every async receiver job.
//! Consumers receive only a weak cache/resolver handle, so keeping the SDK
//! integration alive cannot keep the network daemon alive by accident.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle as ThreadJoinHandle};
use std::time::{Duration, Instant};

use hass_core::{Error as CoreError, Result as CoreResult};
use mdns_sd::{
    DaemonEvent, DaemonStatus, MDNS_PORT, NetworkInterface, RawTxtProperty, ResolvedService,
    ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo, UnregisterStatus,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle as TokioJoinHandle;

use crate::{
    RawResolvedServiceInfo, RawTxtEntry, ResolverFuture, ScopedIpAddress, ServiceStateChange,
    ZeroconfBackend,
};

const REGISTER_ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const DROP_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const COMMAND_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(5);
type BrowseCallback = Arc<dyn Fn(&str, &str, ServiceStateChange) -> CoreResult<()> + Send + Sync>;

/// Start an owned daemon on exactly the supplied interface snapshot and UDP
/// port. This does not start browsing; the owner chooses its browse types later.
pub fn start(interfaces: Vec<NetworkInterface>, port: u16) -> CoreResult<MdnsWireOwner> {
    MdnsWireOwner::start(interfaces, port)
}

/// Strong owner for the native mDNS daemon and its receiver jobs.
///
/// Call [`stop_browsing`](Self::stop_browsing) when the SDK stops, then
/// [`shutdown`](Self::shutdown) at final application close. Dropping without
/// async shutdown requests goodbyes, waits up to two seconds for their status,
/// then waits up to two seconds for daemon shutdown status. If the daemon has
/// not stopped by then, a reaper retains the real
/// worker handle and joins it after the daemon exits. That fallback can outlive
/// the owner if the underlying daemon never exits; explicit async shutdown is
/// the path that reports status and join failures to the caller.
pub struct MdnsWireOwner {
    daemon: ServiceDaemon,
    daemon_worker: Mutex<Option<ThreadJoinHandle<()>>>,
    backend_state: Arc<BackendState>,
    browsers: Mutex<BrowserJobs>,
    registrations: Arc<Mutex<RegistrationState>>,
    monitor_job: Mutex<Option<TokioJoinHandle<()>>>,
    shutdown_started: AtomicBool,
    shutdown_complete: AtomicBool,
}

impl MdnsWireOwner {
    /// Create the daemon using only `interfaces`, before any browsing begins.
    ///
    /// Production uses the standard mDNS port. Tests pass an isolated nonzero
    /// port to every loopback-only role instead.
    pub fn start(interfaces: Vec<NetworkInterface>, port: u16) -> CoreResult<Self> {
        if port == 0 {
            return Err(CoreError::InvalidInput(format!(
                "mDNS requires a nonzero UDP port (normally {MDNS_PORT})"
            )));
        }

        let (daemon, daemon_worker) = ServiceDaemon::new_with_interfaces(interfaces, port)
            .map_err(|error| CoreError::Service(format!("could not start mDNS daemon: {error}")))?;

        Ok(Self {
            daemon,
            daemon_worker: Mutex::new(Some(daemon_worker)),
            backend_state: Arc::new(BackendState::default()),
            browsers: Mutex::new(BrowserJobs::default()),
            registrations: Arc::new(Mutex::new(RegistrationState::default())),
            monitor_job: Mutex::new(None),
            shutdown_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
        })
    }

    /// Return a weak cache/resolver implementation for the shared SDK.
    pub fn backend(&self) -> Arc<MdnsWireBackend> {
        Arc::new(MdnsWireBackend {
            state: Arc::downgrade(&self.backend_state),
        })
    }

    /// Browse only the service types supplied by the SDK's generated registry.
    ///
    /// The callback runs synchronously after the backend mirror has been
    /// updated. It must not retain this owner; the usual SDK callback captures
    /// only its weak manager handle.
    pub fn start_browsing<I, S, F>(&self, service_types: I, callback: F) -> CoreResult<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: Fn(&str, &str, ServiceStateChange) -> CoreResult<()> + Send + Sync + 'static,
    {
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::Service(
                "cannot start mDNS browsing after shutdown has begun".to_owned(),
            ));
        }

        let mut types = Vec::new();
        for service_type in service_types {
            let service_type = service_type.as_ref().to_owned();
            if !types
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(&service_type))
            {
                types.push(service_type);
            }
        }
        if types.is_empty() {
            return Ok(());
        }

        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            CoreError::Service(format!("mDNS browsing requires a Tokio runtime: {error}"))
        })?;
        {
            let mut browsers = lock_unpoisoned(&self.browsers);
            if browsers.started || browsers.stopped {
                return Err(CoreError::Service(
                    "mDNS browsing can be started once per wire owner".to_owned(),
                ));
            }
            browsers.started = true;
        }

        let mut receivers = Vec::with_capacity(types.len());
        for service_type in &types {
            match enqueue_command(|| self.daemon.browse(service_type)) {
                Ok(receiver) => receivers.push((service_type.clone(), receiver)),
                Err(error) => {
                    for started_type in &types[..receivers.len()] {
                        if let Err(stop_error) =
                            enqueue_command(|| self.daemon.stop_browse(started_type))
                        {
                            tracing::warn!(
                                service_type = started_type,
                                error = %stop_error,
                                "could not stop a partially started mDNS browse"
                            );
                        }
                    }
                    lock_unpoisoned(&self.browsers).started = false;
                    return Err(CoreError::Service(format!(
                        "could not browse {service_type}: {error}"
                    )));
                }
            }
        }

        let callback: BrowseCallback = Arc::new(callback);
        let mut jobs = Vec::with_capacity(receivers.len());
        for (service_type, receiver) in receivers {
            let state = Arc::clone(&self.backend_state);
            let callback = Arc::clone(&callback);
            jobs.push(runtime.spawn(async move {
                receive_browse_events(service_type, receiver, state, callback).await;
            }));
        }

        let mut browsers = lock_unpoisoned(&self.browsers);
        if browsers.stopped {
            drop(browsers);
            for service_type in &types {
                let _ = enqueue_command(|| self.daemon.stop_browse(service_type));
            }
            for job in &jobs {
                job.abort();
            }
            return Err(CoreError::Service(
                "mDNS browsing was stopped while it was starting".to_owned(),
            ));
        }
        browsers.types = types;
        browsers.jobs = jobs;
        Ok(())
    }

    /// Stop every active browse and abort/join its Tokio receiver jobs without
    /// shutting down the daemon. This is safe before advertisement cleanup.
    pub async fn stop_browsing(&self) -> CoreResult<()> {
        let (types, jobs) = {
            let mut browsers = lock_unpoisoned(&self.browsers);
            browsers.stopped = true;
            (
                std::mem::take(&mut browsers.types),
                std::mem::take(&mut browsers.jobs),
            )
        };

        let mut first_error = None;
        for service_type in types {
            if let Err(error) = enqueue_command(|| self.daemon.stop_browse(&service_type))
                && !matches!(error, mdns_sd::Error::DaemonShutdown)
            {
                tracing::warn!(
                    service_type,
                    error = %error,
                    "could not stop mDNS browse cleanly"
                );
                first_error.get_or_insert_with(|| {
                    CoreError::Service(format!("could not stop mDNS browse: {error}"))
                });
            }
        }

        for job in &jobs {
            job.abort();
        }
        for job in jobs {
            if let Err(error) = job.await
                && !error.is_cancelled()
            {
                tracing::error!(error = %error, "mDNS browse receiver task failed");
                first_error.get_or_insert_with(|| {
                    CoreError::Service(format!("mDNS browse receiver failed: {error}"))
                });
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    /// Register a service with conflict probing enabled and return its actual
    /// announced fullname after the daemon reports any conflict rename.
    pub async fn register_service(&self, mut service: ServiceInfo) -> CoreResult<String> {
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::Service(
                "cannot register an mDNS service after shutdown has begun".to_owned(),
            ));
        }
        self.ensure_daemon_monitor()?;

        // Source registrations allow conflict-driven instance renaming. Keep
        // upstream's real probe enabled even if the caller supplied a modified
        // ServiceInfo value.
        service.set_requires_probe(true);

        let original = service.get_fullname().to_owned();
        let key = canonical(&original);
        let (notify, announced) = oneshot::channel();
        {
            let mut registrations = lock_unpoisoned(&self.registrations);
            if registrations.entries.contains_key(&key) {
                return Err(CoreError::Service(format!(
                    "mDNS service is already owned by this wire adapter: {original}"
                )));
            }
            registrations.entries.insert(
                key.clone(),
                Registration {
                    original: original.clone(),
                    current: original.clone(),
                },
            );
            registrations.aliases.insert(key.clone(), key.clone());
            registrations.pending.insert(key.clone(), notify);
        }

        if let Err(error) = enqueue_command(|| self.daemon.register(service.clone())) {
            remove_registration(&self.registrations, &key);
            return Err(CoreError::Service(format!(
                "could not register {original}: {error}"
            )));
        }

        match tokio::time::timeout(REGISTER_ANNOUNCE_TIMEOUT, announced).await {
            Ok(Ok(actual)) => Ok(actual),
            Ok(Err(_)) => {
                let cleanup = self.unregister_service(&original).await;
                if matches!(&cleanup, Ok(()) | Err(CoreError::NotFound(_))) {
                    remove_registration(&self.registrations, &key);
                } else if let Err(error) = cleanup {
                    tracing::warn!(fullname = original, error = %error, "could not clean up an mDNS registration after monitor closure");
                }
                Err(CoreError::Service(format!(
                    "mDNS monitor stopped before {original} was announced"
                )))
            }
            Err(_) => {
                // Keep the owner map long enough for unregister_service to use
                // the original daemon key even if the name was already changed.
                match self.unregister_service(&original).await {
                    Ok(()) | Err(CoreError::NotFound(_)) => {
                        remove_registration(&self.registrations, &key);
                    }
                    Err(error) => {
                        tracing::warn!(fullname = original, error = %error, "could not clean up a timed-out mDNS registration");
                    }
                }
                Err(CoreError::Service(format!(
                    "timed out waiting for mDNS announcement of {original}"
                )))
            }
        }
    }

    /// Gracefully unregister an owned service, waiting for the daemon's status
    /// response. Either the original name or the returned actual name is valid.
    pub async fn unregister_service(&self, fullname: &str) -> CoreResult<()> {
        let query_key = canonical(fullname);
        let (daemon_key, owner_key) = {
            let registrations = lock_unpoisoned(&self.registrations);
            let owner_key = registrations
                .aliases
                .get(&query_key)
                .cloned()
                .unwrap_or_else(|| query_key.clone());
            let daemon_key = registrations
                .entries
                .get(&owner_key)
                .map(|registration| registration.original.clone())
                .unwrap_or_else(|| fullname.to_owned());
            (daemon_key, owner_key)
        };

        let receiver =
            enqueue_command(|| self.daemon.unregister(&daemon_key)).map_err(|error| {
                CoreError::Service(format!("could not unregister {fullname}: {error}"))
            })?;
        let status = tokio::time::timeout(UNREGISTER_TIMEOUT, receiver.recv_async())
            .await
            .map_err(|_| {
                CoreError::Service(format!("timed out unregistering mDNS service {fullname}"))
            })?
            .map_err(|error| {
                CoreError::Service(format!("mDNS unregister status channel closed: {error}"))
            })?;

        match status {
            UnregisterStatus::OK => {
                remove_registration(&self.registrations, &owner_key);
                self.stop_monitor_if_unused().await;
                Ok(())
            }
            UnregisterStatus::NotFound => Err(CoreError::NotFound(format!(
                "mDNS service registration {fullname}"
            ))),
        }
    }

    /// Stop browsing, send goodbye records for owned advertisements, wait for
    /// daemon shutdown status, and then join the daemon's real std thread.
    pub async fn shutdown(&self) -> CoreResult<()> {
        if self.shutdown_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        if self
            .shutdown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(CoreError::Service(
                "mDNS wire shutdown is already in progress".to_owned(),
            ));
        }

        if let Err(error) = self.stop_browsing().await {
            tracing::warn!(error = %error, "mDNS browsing did not stop cleanly before shutdown");
        }
        self.backend_state.close();

        let registrations: Vec<String> = lock_unpoisoned(&self.registrations)
            .entries
            .values()
            .map(|registration| registration.original.clone())
            .collect();
        for fullname in registrations {
            if let Err(error) = self.unregister_service(&fullname).await {
                tracing::warn!(fullname, error = %error, "could not unregister mDNS service during shutdown");
            }
        }

        self.wait_for_daemon_shutdown().await?;
        self.join_monitor().await;

        let worker = lock_unpoisoned(&self.daemon_worker).take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|error| {
                    CoreError::Service(format!("mDNS daemon join task failed: {error}"))
                })?
                .map_err(|_| CoreError::Service("mDNS daemon thread panicked".to_owned()))?;
        }
        self.shutdown_complete.store(true, Ordering::Release);
        Ok(())
    }

    fn ensure_daemon_monitor(&self) -> CoreResult<()> {
        let mut monitor = lock_unpoisoned(&self.monitor_job);
        if monitor.as_ref().is_some_and(|job| !job.is_finished()) {
            return Ok(());
        }
        if monitor.take().is_some() {
            tracing::warn!("restarting a finished mDNS monitor receiver");
        }

        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            CoreError::Service(format!(
                "mDNS advertisement requires a Tokio runtime: {error}"
            ))
        })?;
        let receiver = enqueue_command(|| self.daemon.monitor()).map_err(|error| {
            CoreError::Service(format!("could not monitor mDNS daemon events: {error}"))
        })?;
        let registrations = Arc::clone(&self.registrations);
        *monitor = Some(runtime.spawn(async move {
            receive_daemon_events(receiver, registrations).await;
        }));
        Ok(())
    }

    async fn stop_monitor_if_unused(&self) {
        let should_stop = {
            let registrations = lock_unpoisoned(&self.registrations);
            registrations.entries.is_empty() && registrations.pending.is_empty()
        };
        if !should_stop {
            return;
        }
        let job = lock_unpoisoned(&self.monitor_job).take();
        if let Some(job) = job {
            job.abort();
            let _ = job.await;
        }
    }

    async fn join_monitor(&self) {
        let Some(mut job) = lock_unpoisoned(&self.monitor_job).take() else {
            return;
        };
        if tokio::time::timeout(Duration::from_secs(5), &mut job)
            .await
            .is_err()
        {
            job.abort();
            let _ = job.await;
        }
    }

    async fn wait_for_daemon_shutdown(&self) -> CoreResult<()> {
        let deadline = Instant::now() + DAEMON_SHUTDOWN_TIMEOUT;
        loop {
            match self.daemon.shutdown() {
                Ok(receiver) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let status = tokio::time::timeout(remaining, receiver.recv_async())
                        .await
                        .map_err(|_| {
                            CoreError::Service(
                                "timed out waiting for mDNS daemon shutdown".to_owned(),
                            )
                        })?
                        .map_err(|error| {
                            CoreError::Service(format!(
                                "mDNS daemon status channel closed: {error}"
                            ))
                        })?;
                    return match status {
                        DaemonStatus::Shutdown => Ok(()),
                        DaemonStatus::Running => Err(CoreError::Service(
                            "mDNS daemon reported Running after shutdown".to_owned(),
                        )),
                        _ => Err(CoreError::Service(
                            "mDNS daemon returned an unknown shutdown status".to_owned(),
                        )),
                    };
                }
                Err(mdns_sd::Error::Again) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(mdns_sd::Error::DaemonShutdown) => {
                    // The worker has already exited; joining below still proves
                    // its actual thread has terminated.
                    return Ok(());
                }
                Err(error) => {
                    return Err(CoreError::Service(format!(
                        "could not request mDNS daemon shutdown: {error}"
                    )));
                }
            }
        }
    }
}

impl Drop for MdnsWireOwner {
    fn drop(&mut self) {
        if self.shutdown_complete.load(Ordering::Acquire) {
            return;
        }

        self.backend_state.close();
        let (types, jobs) = {
            let mut browsers = lock_unpoisoned(&self.browsers);
            browsers.stopped = true;
            (
                std::mem::take(&mut browsers.types),
                std::mem::take(&mut browsers.jobs),
            )
        };
        for service_type in types {
            let _ = self.daemon.stop_browse(&service_type);
        }
        for job in jobs {
            job.abort();
        }
        if let Some(job) = lock_unpoisoned(&self.monitor_job).take() {
            job.abort();
        }

        let unregister_names: Vec<String> = lock_unpoisoned(&self.registrations)
            .entries
            .values()
            .map(|registration| registration.original.clone())
            .collect();
        let unregister_deadline = Instant::now() + DROP_SHUTDOWN_WAIT;
        for fullname in unregister_names {
            loop {
                match self.daemon.unregister(&fullname) {
                    Ok(receiver) => {
                        let remaining =
                            unregister_deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            tracing::warn!(
                                fullname,
                                "mDNS Drop cleanup could not wait for goodbye status"
                            );
                        } else if let Err(error) = receiver.recv_timeout(remaining) {
                            tracing::warn!(fullname, error = %error, "mDNS Drop cleanup did not receive goodbye status");
                        }
                        break;
                    }
                    Err(mdns_sd::Error::Again) if Instant::now() < unregister_deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        tracing::warn!(fullname, error = %error, "mDNS Drop cleanup could not unregister service");
                        break;
                    }
                }
            }
            if Instant::now() >= unregister_deadline {
                break;
            }
        }

        let Some(worker) = lock_unpoisoned(&self.daemon_worker).take() else {
            return;
        };
        match self.daemon.shutdown() {
            Ok(receiver) => match receiver.recv_timeout(DROP_SHUTDOWN_WAIT) {
                Ok(DaemonStatus::Shutdown) | Err(mdns_sd::RecvTimeoutError::Disconnected) => {
                    if worker.join().is_err() {
                        tracing::error!("mDNS daemon thread panicked during Drop cleanup");
                    }
                }
                Ok(DaemonStatus::Running) | Err(mdns_sd::RecvTimeoutError::Timeout) => {
                    spawn_daemon_reaper(self.daemon.clone(), worker);
                }
                Ok(_) => spawn_daemon_reaper(self.daemon.clone(), worker),
            },
            Err(mdns_sd::Error::DaemonShutdown) => {
                if worker.join().is_err() {
                    tracing::error!("mDNS daemon thread panicked during Drop cleanup");
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "mDNS Drop cleanup could not request immediate shutdown");
                spawn_daemon_reaper(self.daemon.clone(), worker);
            }
        }
    }
}

/// Weak resolver/cache handle. It never extends the wire owner's lifetime.
pub struct MdnsWireBackend {
    state: Weak<BackendState>,
}

impl ZeroconfBackend for MdnsWireBackend {
    fn complete_cached(&self, service_type: &str, name: &str) -> Option<RawResolvedServiceInfo> {
        let state = self.state.upgrade()?;
        lock_unpoisoned(&state.cache)
            .records
            .get(&ServiceKey::new(service_type, name))
            .cloned()
    }

    fn cached_names(&self, service_type: &str) -> Vec<String> {
        let Some(state) = self.state.upgrade() else {
            return Vec::new();
        };
        lock_unpoisoned(&state.cache)
            .found
            .iter()
            .filter(|(key, _)| key.service_type.eq_ignore_ascii_case(service_type))
            .map(|(_, name)| name.clone())
            .collect()
    }

    fn resolve(&self, service_type: String, name: String) -> ResolverFuture {
        let state = self.state.clone();
        Box::pin(async move {
            let Some(state) = state.upgrade() else {
                return Ok(None);
            };
            let key = ServiceKey::new(&service_type, &name);
            let (receiver, id) = {
                let mut cache = lock_unpoisoned(&state.cache);
                if cache.closed {
                    return Ok(None);
                }
                if let Some(record) = cache.records.get(&key) {
                    return Ok(Some(record.clone()));
                }

                let id = cache.next_waiter_id;
                cache.next_waiter_id = cache.next_waiter_id.wrapping_add(1).max(1);
                let (sender, receiver) = oneshot::channel();
                cache
                    .waiters
                    .entry(key.clone())
                    .or_default()
                    .insert(id, sender);
                (receiver, id)
            };
            let _registration = WaiterRegistration {
                state: Arc::downgrade(&state),
                key,
                id,
            };
            Ok(receiver.await.unwrap_or(None))
        })
    }
}

#[derive(Default)]
struct BackendState {
    cache: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    records: BTreeMap<ServiceKey, RawResolvedServiceInfo>,
    found: BTreeMap<ServiceKey, String>,
    waiters: BTreeMap<ServiceKey, BTreeMap<u64, oneshot::Sender<Option<RawResolvedServiceInfo>>>>,
    next_waiter_id: u64,
    closed: bool,
}

impl BackendState {
    fn found(&self, service_type: &str, name: &str) -> ServiceStateChange {
        let key = ServiceKey::new(service_type, name);
        let mut cache = lock_unpoisoned(&self.cache);
        let previous = cache.found.insert(key, name.to_owned());
        if previous.is_some() {
            ServiceStateChange::Updated
        } else {
            ServiceStateChange::Added
        }
    }

    fn resolved(&self, record: RawResolvedServiceInfo) {
        let key = ServiceKey::new(&record.service_type, &record.name);
        let waiters = {
            let mut cache = lock_unpoisoned(&self.cache);
            cache.found.insert(key.clone(), record.name.clone());
            cache.records.insert(key.clone(), record.clone());
            cache.waiters.remove(&key).unwrap_or_default()
        };
        for (_, waiter) in waiters {
            let _ = waiter.send(Some(record.clone()));
        }
    }

    fn removed(&self, service_type: &str, name: &str) {
        let key = ServiceKey::new(service_type, name);
        let waiters = {
            let mut cache = lock_unpoisoned(&self.cache);
            cache.records.remove(&key);
            cache.found.remove(&key);
            cache.waiters.remove(&key).unwrap_or_default()
        };
        for (_, waiter) in waiters {
            let _ = waiter.send(None);
        }
    }

    fn close(&self) {
        let waiters = {
            let mut cache = lock_unpoisoned(&self.cache);
            cache.closed = true;
            std::mem::take(&mut cache.waiters)
        };
        for (_, waiters) in waiters {
            for (_, waiter) in waiters {
                let _ = waiter.send(None);
            }
        }
    }
}

struct WaiterRegistration {
    state: Weak<BackendState>,
    key: ServiceKey,
    id: u64,
}

impl Drop for WaiterRegistration {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut cache = lock_unpoisoned(&state.cache);
        if let Some(waiters) = cache.waiters.get_mut(&self.key) {
            waiters.remove(&self.id);
            if waiters.is_empty() {
                cache.waiters.remove(&self.key);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ServiceKey {
    service_type: String,
    name: String,
}

impl ServiceKey {
    fn new(service_type: &str, name: &str) -> Self {
        Self {
            service_type: canonical(service_type),
            name: canonical(name),
        }
    }
}

fn canonical(value: &str) -> String {
    value.to_ascii_lowercase()
}

/// The daemon's control queue is bounded at 100 commands; the generated
/// registry alone has more service types. Retry only its explicit EAGAIN
/// result, with a deadline. Its independent OS worker drains the queue.
fn enqueue_command<T>(mut command: impl FnMut() -> mdns_sd::Result<T>) -> mdns_sd::Result<T> {
    let deadline = Instant::now() + COMMAND_ENQUEUE_TIMEOUT;
    loop {
        match command() {
            Err(mdns_sd::Error::Again) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            result => return result,
        }
    }
}

#[derive(Default)]
struct BrowserJobs {
    started: bool,
    stopped: bool,
    types: Vec<String>,
    jobs: Vec<TokioJoinHandle<()>>,
}

async fn receive_browse_events(
    _browse_type: String,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    state: Arc<BackendState>,
    callback: BrowseCallback,
) {
    loop {
        let event = match receiver.recv_async().await {
            Ok(event) => event,
            Err(error) => {
                tracing::debug!(error = %error, "mDNS browse receiver closed");
                return;
            }
        };
        match event {
            ServiceEvent::ServiceFound(service_type, name) => {
                let state_change = state.found(&service_type, &name);
                call_update(callback.as_ref(), &service_type, &name, state_change);
            }
            ServiceEvent::ServiceResolved(resolved) => {
                let service_type = resolved.ty_domain.clone();
                let name = resolved.get_fullname().to_owned();
                if let Some(record) = raw_resolved_service(&resolved) {
                    // Cache and wake waiters before manager callbacks, so their
                    // complete_cached fast path sees this exact resolved event.
                    state.resolved(record);
                } else {
                    state.found(&service_type, &name);
                    tracing::debug!(
                        service_type,
                        name,
                        "mDNS returned an incomplete resolved-service event"
                    );
                }
                call_update(
                    callback.as_ref(),
                    &service_type,
                    &name,
                    ServiceStateChange::Updated,
                );
            }
            ServiceEvent::ServiceRemoved(service_type, name) => {
                state.removed(&service_type, &name);
                call_update(
                    callback.as_ref(),
                    &service_type,
                    &name,
                    ServiceStateChange::Removed,
                );
            }
            ServiceEvent::SearchStarted(_) | ServiceEvent::SearchStopped(_) => {}
            _ => {}
        }
    }
}

fn raw_resolved_service(resolved: &ResolvedService) -> Option<RawResolvedServiceInfo> {
    if !resolved.is_valid() {
        return None;
    }

    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    for scoped in resolved.get_addresses_in_cache_order() {
        let address = match scoped {
            ScopedIp::V4(address) => ScopedIpAddress::new(IpAddr::V4(*address.addr()), 0),
            ScopedIp::V6(address) => {
                ScopedIpAddress::new(IpAddr::V6(*address.addr()), address.scope_id().index)
            }
            _ => continue,
        };
        if address.address.is_ipv4() {
            ipv4.push(address);
        } else {
            ipv6.push(address);
        }
    }
    ipv4.extend(ipv6);
    if ipv4.is_empty() {
        return None;
    }

    let txt_entries = resolved
        .get_raw_txt_properties()
        .iter()
        .map(raw_txt_entry)
        .collect();

    Some(RawResolvedServiceInfo {
        ip_addresses: ipv4,
        port: Some(resolved.get_port()),
        hostname: Some(resolved.get_hostname().to_owned()),
        service_type: resolved.ty_domain.clone(),
        name: resolved.get_fullname().to_owned(),
        txt_entries,
    })
}

fn raw_txt_entry(property: &RawTxtProperty) -> RawTxtEntry {
    RawTxtEntry {
        key: property.key().to_vec(),
        value: property.value().map(<[u8]>::to_vec),
    }
}

fn call_update(
    callback: &dyn Fn(&str, &str, ServiceStateChange) -> CoreResult<()>,
    service_type: &str,
    name: &str,
    state: ServiceStateChange,
) {
    if let Err(error) = callback(service_type, name, state) {
        tracing::warn!(service_type, name, ?state, error = %error, "mDNS SDK callback failed");
    }
}

#[derive(Default)]
struct RegistrationState {
    entries: BTreeMap<String, Registration>,
    /// Original and conflict-renamed names both resolve to the original daemon key.
    aliases: HashMap<String, String>,
    pending: HashMap<String, oneshot::Sender<String>>,
}

struct Registration {
    original: String,
    current: String,
}

async fn receive_daemon_events(
    receiver: mdns_sd::Receiver<DaemonEvent>,
    registrations: Arc<Mutex<RegistrationState>>,
) {
    loop {
        let event = match receiver.recv_async().await {
            Ok(event) => event,
            Err(error) => {
                tracing::debug!(error = %error, "mDNS daemon monitor receiver closed");
                return;
            }
        };
        match event {
            DaemonEvent::NameChange(change)
                if matches!(change.rr_type, mdns_sd::RRType::SRV | mdns_sd::RRType::TXT) =>
            {
                let mut registrations = lock_unpoisoned(&registrations);
                let original_key = canonical(&change.original);
                let registration_key = registrations
                    .aliases
                    .get(&original_key)
                    .cloned()
                    .unwrap_or(original_key);
                if let Some(registration) = registrations.entries.get_mut(&registration_key) {
                    registration.current = change.new_name.clone();
                    registrations
                        .aliases
                        .insert(canonical(&change.new_name), registration_key);
                }
                tracing::info!(
                    original = change.original,
                    actual = change.new_name,
                    interface = change.intf_name,
                    "mDNS service name changed after conflict probing"
                );
            }
            DaemonEvent::Announce(fullname, _) => {
                let mut registrations = lock_unpoisoned(&registrations);
                let fullname_key = canonical(&fullname);
                let registration_key =
                    registrations
                        .aliases
                        .get(&fullname_key)
                        .cloned()
                        .or_else(|| {
                            registrations
                                .entries
                                .contains_key(&fullname_key)
                                .then_some(fullname_key.clone())
                        });
                if let Some(registration_key) = registration_key {
                    if let Some(registration) = registrations.entries.get_mut(&registration_key) {
                        registration.current = fullname.clone();
                    }
                    registrations
                        .aliases
                        .insert(fullname_key, registration_key.clone());
                    if let Some(pending) = registrations.pending.remove(&registration_key) {
                        let _ = pending.send(fullname);
                    }
                }
            }
            DaemonEvent::Error(error) => {
                tracing::warn!(error = %error, "native mDNS daemon reported an error");
            }
            DaemonEvent::IpAdd(address) => {
                tracing::debug!(%address, "native mDNS daemon observed an address addition");
            }
            DaemonEvent::IpDel(address) => {
                tracing::debug!(%address, "native mDNS daemon observed an address removal");
            }
            DaemonEvent::Respond(_) => {}
            _ => {}
        }
    }
}

fn remove_registration(registrations: &Mutex<RegistrationState>, owner_key: &str) {
    let mut registrations = lock_unpoisoned(registrations);
    registrations.entries.remove(owner_key);
    registrations.pending.remove(owner_key);
    registrations
        .aliases
        .retain(|_, target| target != owner_key);
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_daemon_reaper(daemon: ServiceDaemon, worker: ThreadJoinHandle<()>) {
    let shared_worker = Arc::new(Mutex::new(Some(worker)));
    let reaper_worker = Arc::clone(&shared_worker);
    let fallback_daemon = daemon.clone();
    let reaper_daemon = daemon;
    let reaper = thread::Builder::new()
        .name("hass-zeroconf-daemon-reaper".to_owned())
        .spawn(move || {
            let mut status_receiver = None;
            loop {
                match reaper_daemon.shutdown() {
                    Ok(receiver) => {
                        status_receiver = Some(receiver);
                        break;
                    }
                    Err(mdns_sd::Error::Again) => thread::sleep(Duration::from_millis(10)),
                    Err(mdns_sd::Error::DaemonShutdown) => break,
                    Err(error) => {
                        tracing::warn!(error = %error, "mDNS daemon reaper could not request shutdown");
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            if let Some(receiver) = status_receiver {
                loop {
                    match receiver.recv() {
                        Ok(DaemonStatus::Shutdown) | Err(_) => break,
                        Ok(DaemonStatus::Running) => {
                            tracing::error!("mDNS daemon reaper received Running after shutdown request");
                        }
                        Ok(_) => break,
                    }
                }
            }
            let worker = lock_unpoisoned(&reaper_worker).take();
            if worker.is_some_and(|worker| worker.join().is_err()) {
                tracing::error!("mDNS daemon thread panicked in its Drop reaper");
            }
        });
    if let Err(error) = reaper {
        // Retain the real worker handle in a final synchronous fallback. The
        // ordinary path is bounded by the reaper's shutdown retry window.
        tracing::error!(error = %error, "could not spawn mDNS daemon reaper; joining synchronously");
        // `JoinHandle::join` cannot be timed out; this exceptional path avoids
        // detaching a live network worker if the process is out of thread slots.
        loop {
            match fallback_daemon.shutdown() {
                Ok(receiver) => {
                    loop {
                        match receiver.recv() {
                            Ok(DaemonStatus::Shutdown) | Err(_) => break,
                            Ok(DaemonStatus::Running) => {
                                tracing::error!(
                                    "mDNS fallback received Running after shutdown request"
                                );
                            }
                            Ok(_) => break,
                        }
                    }
                    break;
                }
                Err(mdns_sd::Error::Again) => thread::sleep(Duration::from_millis(10)),
                Err(mdns_sd::Error::DaemonShutdown) => break,
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
        if let Some(worker) = lock_unpoisoned(&shared_worker).take() {
            let _ = worker.join();
        }
    }
}
