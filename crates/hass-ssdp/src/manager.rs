//! The shared SSDP cache, description pipeline, and owned private callbacks.

use std::{
    collections::BTreeMap,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use hass_core::{Error, Result};
use tokio::task::JoinHandle;

use crate::{
    Description, DescriptionCache, DeviceSnapshot, IntegrationMatchers, IntegrationMatchersConfig,
    MatchDict, SsdpChange, SsdpMessage, SsdpServiceInfo, SsdpSource, Tracker, TrackerEvent,
    headers_match,
};

/// Work sent to the native configuration-flow adapter, never the public event bus.
#[derive(Clone, Debug, PartialEq)]
pub enum DiscoveryAction {
    Create {
        domain: String,
        info: Box<SsdpServiceInfo>,
        udn: String,
    },
    Dismiss {
        ssdp_st: String,
        ssdp_location: Option<String>,
    },
}

pub type DiscoveryFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
pub type DiscoverySink = Arc<dyn Fn(DiscoveryAction) -> DiscoveryFuture + Send + Sync>;
pub type SsdpCallback = Arc<dyn Fn(&SsdpServiceInfo, SsdpChange) -> Result<()> + Send + Sync>;
/// Async callbacks transfer their future into the integration's cancellation scope.
/// The future must own its work; use [`owned_discovery_task`] for spawned tasks.
pub type SsdpAsyncCallback =
    Arc<dyn Fn(SsdpServiceInfo, SsdpChange) -> DiscoveryFuture + Send + Sync>;

struct CallbackRegistration {
    id: u64,
    matcher: MatchDict,
    callback: SsdpCallback,
}

struct Inner {
    active: AtomicBool,
    tracker: Mutex<Tracker>,
    // Preserve one live object identity per tracked device, including references
    // already awaiting HTTP when that device updates or leaves the tracker.
    devices: Mutex<BTreeMap<String, Arc<Mutex<DeviceSnapshot>>>>,
    descriptions: DescriptionCache,
    matchers: IntegrationMatchers,
    callbacks: Mutex<Vec<CallbackRegistration>>,
    next_id: AtomicU64,
    sink: Option<DiscoverySink>,
    jobs: Mutex<Vec<JoinHandle<()>>>,
    runtime: Option<tokio::runtime::Handle>,
}

/// Strong owner. Managers and callback guards do not keep the integration alive.
pub struct SsdpIntegration {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub struct SsdpManager {
    inner: Weak<Inner>,
}

/// Dropping this guard removes exactly one callback without touching its peers.
pub struct SsdpCallbackGuard {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for SsdpCallbackGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            lock(&inner.callbacks).retain(|entry| entry.id != self.id);
        }
    }
}

impl SsdpIntegration {
    /// Source-compatible description requester: ten seconds and unverified TLS.
    pub fn new(matchers: IntegrationMatchersConfig, sink: Option<DiscoverySink>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| Error::Service(error.to_string()))?;
        Ok(Self::with_client(client, matchers, sink))
    }

    pub fn with_client(
        client: reqwest::Client,
        matchers: IntegrationMatchersConfig,
        sink: Option<DiscoverySink>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                active: AtomicBool::new(true),
                tracker: Mutex::new(Tracker::new()),
                devices: Mutex::new(BTreeMap::new()),
                descriptions: DescriptionCache::new(client),
                matchers: IntegrationMatchers::new(matchers),
                callbacks: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                sink,
                jobs: Mutex::new(Vec::new()),
                runtime: tokio::runtime::Handle::try_current().ok(),
            }),
        }
    }

    pub fn manager(&self) -> SsdpManager {
        SsdpManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Stop and join transferred flow work. The transport owner stops its receivers first.
    pub async fn shutdown(self) {
        let jobs = self.inner.stop();
        for job in jobs {
            let _ = job.await;
        }
    }
}

impl Drop for SsdpIntegration {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

impl Inner {
    fn stop(&self) -> Vec<JoinHandle<()>> {
        let mut jobs = lock(&self.jobs);
        self.active.store(false, Ordering::Release);
        lock(&self.callbacks).clear();
        let jobs = std::mem::take(&mut *jobs);
        for job in &jobs {
            job.abort();
        }
        jobs
    }

    fn dispatch(&self, action: DiscoveryAction) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(sink) = self.sink.clone() else {
            return;
        };
        // Admission is synchronous, like discovery_flow.async_create_flow.
        // Do not move this factory into a scheduled task: byebye/alive
        // admission order would then depend on executor polling order.
        let work = match catch_unwind(AssertUnwindSafe(|| sink(action))) {
            Ok(work) => work,
            Err(_) => {
                tracing::error!("SSDP discovery admission panicked");
                return;
            }
        };
        if let Err(error) = self.spawn_job(work) {
            tracing::error!(%error, "SSDP discovery could not be scheduled");
        }
    }

    fn spawn_job(&self, work: DiscoveryFuture) -> Result<()> {
        let mut jobs = lock(&self.jobs);
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        let runtime = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or_else(|| Error::Service("SSDP callback requires a Tokio runtime".into()))?;
        jobs.retain(|job| !job.is_finished());
        jobs.push(runtime.spawn(async move {
            let outcome = AssertUnwindSafe(work).catch_unwind().await;
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "SSDP callback work failed"),
                Err(_) => tracing::error!("SSDP callback work panicked"),
            }
        }));
        Ok(())
    }

    fn process(
        &self,
        event: TrackerEvent,
        description: Option<Arc<Description>>,
        skip_callbacks: bool,
    ) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        let headers = event.device.combined_headers(&event.dst);
        let empty = Description::new();
        let description = description.as_deref().unwrap_or(&empty);
        let domains = if event.source == SsdpSource::SearchAlive {
            Default::default()
        } else {
            self.matchers.matching_domains(&headers, description)
        };
        let callbacks: Vec<_> = if skip_callbacks {
            Vec::new()
        } else {
            lock(&self.callbacks)
                .iter()
                .filter(|entry| headers_match(&headers, &entry.matcher))
                .map(|entry| entry.callback.clone())
                .collect()
        };
        if callbacks.is_empty()
            && domains.is_empty()
            && event.source != SsdpSource::AdvertisementByebye
        {
            return Ok(());
        }
        let mut info =
            SsdpServiceInfo::from_headers_and_description(&event.device, &headers, description)?;
        info.x_homeassistant_matching_domains = domains.clone();
        if let Some(change) = SsdpChange::from_source(event.source) {
            for callback in callbacks {
                if !self.active.load(Ordering::Acquire) {
                    return Ok(());
                }
                deliver(&callback, &info, change);
            }
        }
        if event.source == SsdpSource::AdvertisementByebye {
            self.dispatch(DiscoveryAction::Dismiss {
                ssdp_st: info.ssdp_st,
                ssdp_location: info.ssdp_location,
            });
            return Ok(());
        }
        for domain in domains {
            self.dispatch(DiscoveryAction::Create {
                domain,
                info: Box::new(info.clone()),
                udn: event.device.udn.clone(),
            });
        }
        Ok(())
    }
}

impl SsdpManager {
    fn inner(&self) -> Result<Arc<Inner>> {
        self.inner
            .upgrade()
            .filter(|inner| inner.active.load(Ordering::Acquire))
            .ok_or_else(|| Error::Service("SSDP integration has stopped".into()))
    }

    /// Feed all interface listeners into this single tracker.
    pub async fn observe(&self, message: &SsdpMessage, now: DateTime<Utc>) -> Result<bool> {
        let inner = self.inner()?;
        let (event, device) = {
            let mut tracker = lock(&inner.tracker);
            let event = tracker.observation(message, now);
            let mut devices = lock(&inner.devices);
            let device = message.udn.as_deref().and_then(|udn| {
                if let Some(snapshot) = tracker.get(udn) {
                    let handle = devices
                        .entry(udn.to_owned())
                        .or_insert_with(|| Arc::new(Mutex::new(snapshot.clone())))
                        .clone();
                    *lock(&handle) = snapshot.clone();
                    Some(handle)
                } else if let Some(event) = &event {
                    let handle = devices
                        .remove(udn)
                        .unwrap_or_else(|| Arc::new(Mutex::new(event.device.clone())));
                    *lock(&handle) = event.device.clone();
                    Some(handle)
                } else {
                    None
                }
            });
            devices.retain(|udn, _| tracker.get(udn).is_some());
            (event, device)
        };
        let Some(mut event) = event else {
            return Ok(false);
        };
        let device = device.expect("a propagated tracker event has a device");
        // Live callbacks fetch from device.location, which selects the first
        // live location. Getters and registration replay use header LOCATION.
        let location = lock(&device).current_location().map(str::to_owned);
        let description = inner.descriptions.get(location.as_deref()).await;
        event.device = lock(&device).clone();
        inner.process(event, description, false)?;
        Ok(true)
    }

    /// Replay matching cached services as ALIVE, then attach for live delivery.
    pub async fn register_callback(
        &self,
        matcher: MatchDict,
        callback: SsdpCallback,
    ) -> Result<SsdpCallbackGuard> {
        let inner = self.inner()?;
        let devices: Vec<_> = lock(&inner.tracker).devices().values().cloned().collect();
        for device in devices {
            for headers in device.all_combined_headers().into_values() {
                if !headers_match(&headers, &matcher) {
                    continue;
                }
                let description = inner.descriptions.get(headers.get("location")).await;
                if !inner.active.load(Ordering::Acquire) {
                    return Err(Error::Service("SSDP integration has stopped".into()));
                }
                let empty = Description::new();
                let info = SsdpServiceInfo::from_headers_and_description(
                    &device,
                    &headers,
                    description.as_deref().unwrap_or(&empty),
                )?;
                deliver(&callback, &info, SsdpChange::Alive);
            }
        }
        let mut callbacks = lock(&inner.callbacks);
        if !inner.active.load(Ordering::Acquire) {
            return Err(Error::Service("SSDP integration has stopped".into()));
        }
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        callbacks.push(CallbackRegistration {
            id,
            matcher,
            callback,
        });
        Ok(SsdpCallbackGuard {
            inner: Arc::downgrade(&inner),
            id,
        })
    }

    pub async fn register_async_callback(
        &self,
        matcher: MatchDict,
        callback: SsdpAsyncCallback,
    ) -> Result<SsdpCallbackGuard> {
        let inner = self.inner.clone();
        self.register_callback(
            matcher,
            Arc::new(move |info, change| {
                let Some(inner) = inner.upgrade() else {
                    return Ok(());
                };
                let callback = callback.clone();
                let info = info.clone();
                inner.spawn_job(Box::pin(async move { callback(info, change).await }))
            }),
        )
        .await
    }

    pub async fn get_by_udn_and_st(&self, udn: &str, st: &str) -> Result<Option<SsdpServiceInfo>> {
        let inner = self.inner()?;
        let device = lock(&inner.tracker).get(udn).cloned();
        let Some(device) = device else {
            return Ok(None);
        };
        if !device.search_headers.contains_key(st) && !device.advertisement_headers.contains_key(st)
        {
            return Ok(None);
        }
        let headers = device.combined_headers(st);
        let description = inner.descriptions.get(headers.get("location")).await;
        let empty = Description::new();
        Ok(Some(SsdpServiceInfo::from_headers_and_description(
            &device,
            &headers,
            description.as_deref().unwrap_or(&empty),
        )?))
    }

    pub async fn get_by_st(&self, st: &str) -> Result<Vec<SsdpServiceInfo>> {
        let inner = self.inner()?;
        let udns: Vec<_> = lock(&inner.tracker)
            .by_type(st)
            .into_iter()
            .map(|device| device.udn.clone())
            .collect();
        let mut result = Vec::new();
        for udn in udns {
            if let Some(info) = self.get_by_udn_and_st(&udn, st).await? {
                result.push(info);
            }
        }
        Ok(result)
    }

    pub async fn get_by_udn(&self, udn: &str) -> Result<Vec<SsdpServiceInfo>> {
        let inner = self.inner()?;
        let device = lock(&inner.tracker).get(udn).cloned();
        let Some(device) = device else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        for st in device.all_combined_headers().into_keys() {
            if let Some(info) = self.get_by_udn_and_st(udn, &st).await? {
                result.push(info);
            }
        }
        Ok(result)
    }

    /// Config-entry removal only reuses ready, nonempty descriptions. It performs
    /// no HTTP request and does not deliver ordinary device callbacks.
    pub fn rediscover_cached_udn(&self, udn: &str) -> Result<()> {
        let inner = self.inner()?;
        let device = lock(&inner.tracker).get(udn).cloned();
        let Some(device) = device else {
            return Ok(());
        };
        for dst in device.all_combined_headers().into_keys() {
            let (ready, description) = inner.descriptions.peek(device.current_location());
            if ready
                && description
                    .as_ref()
                    .is_some_and(|description| !description.is_empty())
            {
                inner.process(
                    TrackerEvent {
                        source: SsdpSource::Search,
                        device: device.clone(),
                        dst,
                    },
                    description,
                    true,
                )?;
            }
        }
        Ok(())
    }
}

/// Transfer a spawned task without Tokio's default detach-on-drop behavior.
pub fn owned_discovery_task(task: JoinHandle<Result<()>>) -> DiscoveryFuture {
    Box::pin(OwnedTask(task))
}

struct OwnedTask(JoinHandle<Result<()>>);
impl Future for OwnedTask {
    type Output = Result<()>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match Pin::new(&mut self.0).poll(cx) {
            std::task::Poll::Ready(Ok(value)) => std::task::Poll::Ready(value),
            std::task::Poll::Ready(Err(error)) => {
                std::task::Poll::Ready(Err(Error::Service(format!("SSDP task failed: {error}"))))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn deliver(callback: &SsdpCallback, info: &SsdpServiceInfo, change: SsdpChange) {
    match catch_unwind(AssertUnwindSafe(|| callback(info, change))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "SSDP callback failed"),
        Err(_) => tracing::error!("SSDP callback panicked"),
    }
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
