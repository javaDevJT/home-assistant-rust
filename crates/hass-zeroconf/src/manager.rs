//! Shared cache-driven Zeroconf discovery and cancellable lookup ownership.

use std::{
    any::Any,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use hass_config::{DiscoveryKey, DiscoveryKeyValue};
use hass_core::{Error, Result};
use tokio::{runtime::Handle, sync::oneshot, task::JoinHandle, time::timeout};

use crate::{
    HOME_ASSISTANT_SERVICE_TYPE, HOMEKIT_SERVICE_TYPES, MatcherIndex, RawZeroconfServiceInfo,
    ZeroconfServiceInfo,
};

/// Match Home Assistant's asynchronous service-info lookup deadline.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Native discovery source identifier for ordinary Zeroconf matcher flows.
pub const ZEROCONF_SOURCE: &str = "zeroconf";

/// Home Assistant's source identifier for flows created by the runtime's
/// HomeKit-specific discovery hook.
pub const HOMEKIT_SOURCE: &str = "homekit";

/// An owned lookup future returned by a DNS-SD backend.
///
/// The future must own all data it needs because the manager transfers it to a
/// Tokio task that can outlive the provider callback that requested it.
pub type ResolverFuture =
    Pin<Box<dyn Future<Output = Result<Option<RawZeroconfServiceInfo>>> + Send + 'static>>;

/// Cache and resolver boundary implemented by the native mDNS adapter.
///
/// `complete_cached` must return only a fully resolved service record. A partial
/// PTR/SRV/TXT cache entry belongs in `cached_names` but returns `None` here so
/// the manager can perform the source-style ten-second lookup.
pub trait ZeroconfBackend: Send + Sync + 'static {
    /// Return a complete resolved record already present in the backend cache.
    fn complete_cached(&self, service_type: &str, name: &str) -> Option<RawZeroconfServiceInfo>;

    /// Return instance names represented by PTR cache records for this type.
    fn cached_names(&self, service_type: &str) -> Vec<String>;

    /// Create an owned lookup future for one type and instance name.
    fn resolve(&self, service_type: String, name: String) -> ResolverFuture;
}

/// Synchronous admission boundary for native config-flow and discovery actions.
///
/// Keep this synchronous: Home Assistant's config-flow helper admits the flow
/// synchronously, preserving ordering against subsequent Removed callbacks.
pub type DiscoveryActionSink = Arc<dyn Fn(DiscoveryAction) -> Result<()> + Send + Sync + 'static>;

/// Typed source attached to Zeroconf config-flow actions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiscoverySource {
    /// An ordinary generated Zeroconf matcher flow.
    Zeroconf,
    /// A flow classified by the runtime's HomeKit discovery hook.
    HomeKit,
}

impl DiscoverySource {
    /// Return Home Assistant's source string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zeroconf => ZEROCONF_SOURCE,
            Self::HomeKit => HOMEKIT_SOURCE,
        }
    }
}

/// Work admitted to the native config-flow/discovery adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryAction {
    /// Create a config flow from a generated service matcher.
    Create {
        /// Integration domain selected by the matcher.
        domain: String,
        /// Source identity used by Home Assistant's config-flow engine.
        source: DiscoverySource,
        /// Provider identity; its domain remains `zeroconf` regardless of the
        /// target integration domain.
        discovery_key: DiscoveryKey,
        /// Converted service info with source-ordered addresses and properties.
        info: Box<ZeroconfServiceInfo>,
        /// Optional HomeKit alternative integration supplied by the runtime hook.
        alternative_domain: Option<String>,
    },
    /// Dismiss active Zeroconf discovery flows by instance name only.
    Dismiss {
        /// Instance name; removal intentionally has no service-type scope.
        name: String,
        /// Discovery source whose active flows should be dismissed.
        source: DiscoverySource,
    },
}

/// Added, Updated, or Removed input from the native DNS-SD browser.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServiceStateChange {
    /// A service instance became visible.
    Added,
    /// A visible service instance changed.
    Updated,
    /// A service instance was removed or sent goodbye.
    Removed,
}

/// Raw update listener called before address validation and matcher dispatch.
pub type RawServiceUpdateListener =
    Arc<dyn Fn(&RawZeroconfServiceInfo) -> Result<()> + Send + Sync + 'static>;

/// Name-only service removal listener.
pub type RawServiceRemovedListener = Arc<dyn Fn(&str) -> Result<()> + Send + Sync + 'static>;

struct UpdateRegistration {
    id: u64,
    callback: RawServiceUpdateListener,
}

struct RemovedRegistration {
    id: u64,
    callback: RawServiceRemovedListener,
}

#[derive(Default)]
struct Listeners {
    updates: Vec<UpdateRegistration>,
    removals: Vec<RemovedRegistration>,
}

#[derive(Default)]
struct ResolutionTasks {
    closed: bool,
    jobs: Vec<JoinHandle<()>>,
}

struct Inner {
    active: AtomicBool,
    backend: Arc<dyn ZeroconfBackend>,
    matchers: MatcherIndex,
    sink: Option<DiscoveryActionSink>,
    runtime: Handle,
    listeners: Mutex<Listeners>,
    tasks: Mutex<ResolutionTasks>,
    next_listener_id: AtomicU64,
}

/// Strong runtime owner for the shared Zeroconf manager and resolution tasks.
///
/// The runtime should keep this owner alive until normal system teardown. Raw
/// update listeners are the intentional extension point for HomeKit
/// classification, duplicate Home Assistant UUID repair checks, and raw API
/// subscriptions; those policies do not live in ordinary matcher dispatch.
pub struct ZeroconfIntegration {
    inner: Arc<Inner>,
}

/// Weak public handle for cache access, rediscovery, and listener registration.
#[derive(Clone)]
pub struct ZeroconfManager {
    inner: Weak<Inner>,
}

/// Dropping a listener guard unregisters exactly the listener it owns.
pub struct ListenerGuard {
    inner: Weak<Inner>,
    id: u64,
    update: bool,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut listeners = lock(&inner.listeners);
        if self.update {
            listeners
                .updates
                .retain(|registration| registration.id != self.id);
        } else {
            listeners
                .removals
                .retain(|registration| registration.id != self.id);
        }
    }
}

impl ZeroconfIntegration {
    /// Create the shared integration using the current Tokio runtime and the
    /// bundled generated matcher registry.
    pub fn new(
        backend: Arc<dyn ZeroconfBackend>,
        sink: Option<DiscoveryActionSink>,
    ) -> Result<Self> {
        let runtime = Handle::try_current().map_err(|error| {
            Error::Service(format!(
                "Zeroconf integration requires a Tokio runtime: {error}"
            ))
        })?;
        Ok(Self::with_matchers(
            backend,
            MatcherIndex::builtin(),
            sink,
            runtime,
        ))
    }

    /// Create the integration with an explicit matcher index and captured Tokio
    /// runtime handle. Useful to embed the manager in a runtime owner.
    pub fn with_matchers(
        backend: Arc<dyn ZeroconfBackend>,
        matchers: MatcherIndex,
        sink: Option<DiscoveryActionSink>,
        runtime: Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                active: AtomicBool::new(true),
                backend,
                matchers,
                sink,
                runtime,
                listeners: Mutex::new(Listeners::default()),
                tasks: Mutex::new(ResolutionTasks::default()),
                next_listener_id: AtomicU64::new(1),
            }),
        }
    }

    /// Return a weak manager handle that does not retain this integration.
    pub fn manager(&self) -> ZeroconfManager {
        ZeroconfManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Abort and join every lookup task owned by this integration.
    ///
    /// The browser/runtime owner should stop delivering new provider callbacks
    /// before calling this method. Dropping the integration still aborts all
    /// tasks as a synchronous fallback, but cannot await their completion.
    pub async fn shutdown(&self) {
        for job in self.inner.stop() {
            if let Err(error) = job.await
                && !error.is_cancelled()
            {
                tracing::error!(error = %error, "Zeroconf lookup task failed while joining");
            }
        }
    }
}

impl Drop for ZeroconfIntegration {
    fn drop(&mut self) {
        // Tokio JoinHandle drop detaches a task, so abort before releasing the
        // handles. Explicit shutdown additionally awaits these aborted tasks.
        drop(self.inner.stop());
    }
}

impl Inner {
    fn stop(&self) -> Vec<JoinHandle<()>> {
        let mut tasks = lock(&self.tasks);
        self.active.store(false, Ordering::Release);
        tasks.closed = true;
        lock(&self.listeners).updates.clear();
        lock(&self.listeners).removals.clear();
        let jobs = std::mem::take(&mut tasks.jobs);
        for job in &jobs {
            job.abort();
        }
        jobs
    }

    fn begin_update(self: &Arc<Self>, service_type: String, name: String) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Err(stopped_error());
        }

        let cached = match catch_unwind(AssertUnwindSafe(|| {
            self.backend.complete_cached(&service_type, &name)
        })) {
            Ok(record) => record,
            Err(payload) => {
                tracing::error!(
                    service_type = %service_type,
                    name = %name,
                    panic = %panic_detail(payload.as_ref()),
                    "Zeroconf backend complete-cache lookup panicked"
                );
                None
            }
        };

        if let Some(record) = cached {
            self.process_raw(record);
            return Ok(());
        }

        self.spawn_lookup(service_type, name)
    }

    fn spawn_lookup(self: &Arc<Self>, service_type: String, name: String) -> Result<()> {
        let mut tasks = lock(&self.tasks);
        if tasks.closed || !self.active.load(Ordering::Acquire) {
            return Err(stopped_error());
        }
        // Completed JoinHandles detach on drop, but completed jobs have no
        // remaining work; prune them as new browser records arrive.
        tasks.jobs.retain(|job| !job.is_finished());

        let weak = Arc::downgrade(self);
        let backend = Arc::clone(&self.backend);
        let lookup_type = service_type.clone();
        let lookup_name = name.clone();
        let (start_task, task_started) = oneshot::channel();
        let task = async move {
            // The handle is registered under the task lock before this gate is
            // opened. No backend call can begin while that lock is held.
            if task_started.await.is_err() {
                return;
            }
            let outcome = AssertUnwindSafe(async move {
                timeout(
                    REQUEST_TIMEOUT,
                    backend.resolve(lookup_type.clone(), lookup_name.clone()),
                )
                .await
            })
            .catch_unwind()
            .await;

            match outcome {
                Ok(Ok(Ok(Some(record)))) => {
                    if let Some(inner) = weak.upgrade() {
                        inner.process_raw(record);
                    }
                }
                Ok(Ok(Ok(None))) => tracing::debug!(
                    service_type = %service_type,
                    name = %name,
                    "Zeroconf lookup completed without a service record"
                ),
                Ok(Ok(Err(error))) => tracing::error!(
                    service_type = %service_type,
                    name = %name,
                    error = %error,
                    "Zeroconf lookup failed"
                ),
                Ok(Err(_elapsed)) => tracing::debug!(
                    service_type = %service_type,
                    name = %name,
                    timeout_seconds = REQUEST_TIMEOUT.as_secs(),
                    "Zeroconf lookup timed out"
                ),
                Err(payload) => tracing::error!(
                    service_type = %service_type,
                    name = %name,
                    panic = %panic_detail(payload.as_ref()),
                    "Zeroconf lookup panicked"
                ),
            }
        };
        tasks.jobs.push(self.runtime.spawn(task));
        drop(tasks);
        let _ = start_task.send(());
        Ok(())
    }

    fn process_raw(&self, record: RawZeroconfServiceInfo) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }

        let update_callbacks: Vec<_> = lock(&self.listeners)
            .updates
            .iter()
            .map(|registration| Arc::clone(&registration.callback))
            .collect();
        for callback in update_callbacks {
            deliver_update(&callback, &record);
        }

        let Some(info) = ZeroconfServiceInfo::from_raw(record) else {
            tracing::debug!("Failed to get usable addresses for Zeroconf service");
            return;
        };

        // HomeKit model classification, alternative-domain filtering, and
        // duplicate UUID repair checks run in raw update listeners registered
        // by the runtime before ordinary matcher dispatch reaches this point.
        let domains: Vec<String> = self
            .matchers
            .matching(&info.service_type, &info)
            .into_iter()
            .map(|matcher| matcher.domain.clone())
            .collect();
        let Some(sink) = &self.sink else {
            return;
        };

        let discovery_key = discovery_key(&info.service_type, &info.name);
        for domain in domains {
            let action = DiscoveryAction::Create {
                domain,
                source: DiscoverySource::Zeroconf,
                discovery_key: discovery_key.clone(),
                info: Box::new(info.clone()),
                alternative_domain: None,
            };
            deliver_action(sink, action);
        }
    }

    fn remove(&self, name: String) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }

        if let Some(sink) = &self.sink {
            deliver_action(
                sink,
                DiscoveryAction::Dismiss {
                    name: name.clone(),
                    source: DiscoverySource::Zeroconf,
                },
            );
        }

        let removal_callbacks: Vec<_> = lock(&self.listeners)
            .removals
            .iter()
            .map(|registration| Arc::clone(&registration.callback))
            .collect();
        for callback in removal_callbacks {
            deliver_remove(&callback, &name);
        }
    }
}

impl ZeroconfManager {
    fn inner(&self) -> Result<Arc<Inner>> {
        self.inner
            .upgrade()
            .filter(|inner| inner.active.load(Ordering::Acquire))
            .ok_or_else(stopped_error)
    }

    /// Generated registry service types, in source table order.
    pub fn service_types(&self) -> Result<Vec<String>> {
        let inner = self.inner()?;
        Ok(inner.matchers.service_types().map(str::to_owned).collect())
    }

    /// Browser types: generated matcher types plus HomeKit and Home Assistant.
    pub fn browse_types(&self) -> Result<Vec<String>> {
        let mut types = self.service_types()?;
        for service_type in HOMEKIT_SERVICE_TYPES
            .into_iter()
            .chain(std::iter::once(HOME_ASSISTANT_SERVICE_TYPE))
        {
            if !types.iter().any(|existing| existing == service_type) {
                types.push(service_type.to_owned());
            }
        }
        Ok(types)
    }

    /// Instance names from the backend cache for one exact service type.
    pub fn cached_names(&self, service_type: &str) -> Result<Vec<String>> {
        let inner = self.inner()?;
        safe_cached_names(&inner, service_type)
    }

    /// Return a complete cached record, if available, without starting a lookup.
    pub fn get_cached(
        &self,
        service_type: &str,
        name: &str,
    ) -> Result<Option<RawZeroconfServiceInfo>> {
        let inner = self.inner()?;
        safe_cached_record(&inner, service_type, name)
    }

    /// Resolve one instance with the source ten-second deadline.
    ///
    /// This future is owned by its caller. Browser updates use the integration's
    /// separate owned task set so provider-thread callbacks never detach work.
    pub async fn resolve(
        &self,
        service_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Option<RawZeroconfServiceInfo>> {
        let backend = {
            let inner = self.inner()?;
            Arc::clone(&inner.backend)
        };
        let service_type = service_type.into();
        let name = name.into();
        let lookup_type = service_type.clone();
        let lookup_name = name.clone();

        match catch_provider_panic(async move {
            timeout(REQUEST_TIMEOUT, backend.resolve(lookup_type, lookup_name)).await
        })
        .await
        {
            Ok(Ok(Ok(record))) => Ok(record),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_elapsed)) => {
                tracing::debug!(
                    service_type = %service_type,
                    name = %name,
                    timeout_seconds = REQUEST_TIMEOUT.as_secs(),
                    "Zeroconf lookup timed out"
                );
                Ok(None)
            }
            Err(panic) => {
                let detail = panic_detail(panic.as_ref());
                tracing::error!(
                    service_type = %service_type,
                    name = %name,
                    panic = %detail,
                    "Zeroconf lookup panicked"
                );
                Err(Error::Service(format!(
                    "Zeroconf lookup panicked: {detail}"
                )))
            }
        }
    }

    /// PTR cache keys for the initial WebSocket snapshot, restricted to the
    /// generated registry service types. Special browser types are excluded.
    pub fn snapshot_names(&self) -> Result<Vec<(String, String)>> {
        let inner = self.inner()?;
        let mut snapshot = Vec::new();
        for service_type in inner.matchers.service_types() {
            for name in safe_cached_names(&inner, service_type)? {
                snapshot.push((service_type.to_owned(), name));
            }
        }
        Ok(snapshot)
    }

    /// Fully resolved records already present in the generated-type cache.
    /// Incomplete PTR entries are available through [`Self::snapshot_names`]
    /// and can be passed to [`Self::resolve`] by an API snapshot task.
    pub fn cached_snapshot(&self) -> Result<Vec<RawZeroconfServiceInfo>> {
        let inner = self.inner()?;
        let mut snapshot = Vec::new();
        for service_type in inner.matchers.service_types() {
            for name in safe_cached_names(&inner, service_type)? {
                if let Some(record) = safe_cached_record(&inner, service_type, &name)? {
                    snapshot.push(record);
                }
            }
        }
        Ok(snapshot)
    }

    /// Process one native browser callback. Cache hits are dispatched
    /// synchronously; misses transfer owned resolution work to the integration.
    pub fn service_update(
        &self,
        service_type: impl Into<String>,
        name: impl Into<String>,
        state: ServiceStateChange,
    ) -> Result<()> {
        let inner = self.inner()?;
        let service_type = service_type.into();
        let name = name.into();
        match state {
            ServiceStateChange::Added | ServiceStateChange::Updated => {
                inner.begin_update(service_type, name)
            }
            ServiceStateChange::Removed => {
                // Source removal is intentionally keyed by instance name only;
                // it does not delete the backend cache record.
                inner.remove(name);
                Ok(())
            }
        }
    }

    /// Replay a config-entry discovery key through the cache/lookup path.
    pub fn rediscover(
        &self,
        service_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<()> {
        self.inner()?.begin_update(service_type.into(), name.into())
    }

    /// Register a raw update observer. It runs before address validation and
    /// generated matcher dispatch, including for unusable-address records.
    pub fn register_service_update_listener<F>(&self, listener: F) -> Result<ListenerGuard>
    where
        F: Fn(&RawZeroconfServiceInfo) -> Result<()> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let mut listeners = lock(&inner.listeners);
        if !inner.active.load(Ordering::Acquire) {
            return Err(stopped_error());
        }
        listeners.updates.push(UpdateRegistration {
            id,
            callback: Arc::new(listener),
        });
        Ok(ListenerGuard {
            inner: Arc::downgrade(&inner),
            id,
            update: true,
        })
    }

    /// Register a name-only removal observer.
    pub fn register_service_removed_listener<F>(&self, listener: F) -> Result<ListenerGuard>
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        let inner = self.inner()?;
        let id = inner.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let mut listeners = lock(&inner.listeners);
        if !inner.active.load(Ordering::Acquire) {
            return Err(stopped_error());
        }
        listeners.removals.push(RemovedRegistration {
            id,
            callback: Arc::new(listener),
        });
        Ok(ListenerGuard {
            inner: Arc::downgrade(&inner),
            id,
            update: false,
        })
    }
}

fn discovery_key(service_type: &str, name: &str) -> DiscoveryKey {
    DiscoveryKey {
        domain: ZEROCONF_SOURCE.to_owned(),
        key: DiscoveryKeyValue::Tuple(vec![service_type.to_owned(), name.to_owned()]),
        version: 1,
    }
}

fn safe_cached_names(inner: &Inner, service_type: &str) -> Result<Vec<String>> {
    match catch_unwind(AssertUnwindSafe(|| {
        inner.backend.cached_names(service_type)
    })) {
        Ok(names) => Ok(names),
        Err(payload) => {
            let detail = panic_detail(payload.as_ref());
            tracing::error!(
                service_type = %service_type,
                panic = %detail,
                "Zeroconf backend cache-name lookup panicked"
            );
            Err(Error::Service(format!(
                "Zeroconf backend cache-name lookup panicked: {detail}"
            )))
        }
    }
}

fn safe_cached_record(
    inner: &Inner,
    service_type: &str,
    name: &str,
) -> Result<Option<RawZeroconfServiceInfo>> {
    match catch_unwind(AssertUnwindSafe(|| {
        inner.backend.complete_cached(service_type, name)
    })) {
        Ok(record) => Ok(record),
        Err(payload) => {
            let detail = panic_detail(payload.as_ref());
            tracing::error!(
                service_type = %service_type,
                name = %name,
                panic = %detail,
                "Zeroconf backend complete-cache lookup panicked"
            );
            Err(Error::Service(format!(
                "Zeroconf backend complete-cache lookup panicked: {detail}"
            )))
        }
    }
}

fn deliver_update(callback: &RawServiceUpdateListener, record: &RawZeroconfServiceInfo) {
    match catch_unwind(AssertUnwindSafe(|| callback(record))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(error = %error, "Zeroconf update listener failed"),
        Err(payload) => tracing::error!(
            panic = %panic_detail(payload.as_ref()),
            "Zeroconf update listener panicked"
        ),
    }
}

fn deliver_remove(callback: &RawServiceRemovedListener, name: &str) {
    match catch_unwind(AssertUnwindSafe(|| callback(name))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(error = %error, "Zeroconf removal listener failed"),
        Err(payload) => tracing::error!(
            panic = %panic_detail(payload.as_ref()),
            "Zeroconf removal listener panicked"
        ),
    }
}

fn deliver_action(sink: &DiscoveryActionSink, action: DiscoveryAction) {
    match catch_unwind(AssertUnwindSafe(|| sink(action))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(error = %error, "Zeroconf discovery action admission failed")
        }
        Err(payload) => tracing::error!(
            panic = %panic_detail(payload.as_ref()),
            "Zeroconf discovery action sink panicked"
        ),
    }
}

async fn catch_provider_panic<T>(future: impl Future<Output = T>) -> std::thread::Result<T> {
    AssertUnwindSafe(future).catch_unwind().await
}

fn panic_detail(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn stopped_error() -> Error {
    Error::Service("Zeroconf integration has stopped".to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
