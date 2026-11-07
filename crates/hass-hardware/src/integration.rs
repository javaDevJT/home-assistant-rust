//! Hardware provider ownership and the shared system-status sampler.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::{HardwareInfo, HardwareProvider, MetricsProvider, SystemStatus};

const DEFAULT_STATUS_CAPACITY: usize = 32;

/// Runtime settings for the shared system-status sampler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HardwareConfig {
    /// Delay between samples. The first sample is also delayed by this amount.
    pub sample_interval: Duration,
    /// Maximum number of status events retained for lagging subscribers.
    pub status_channel_capacity: usize,
}

impl Default for HardwareConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(5),
            status_channel_capacity: DEFAULT_STATUS_CAPACITY,
        }
    }
}

/// Strong owner for the loaded hardware provider registry and sampler task.
///
/// Cloning a [`HardwareManager`] does not keep this integration alive. Dropping
/// this owner requests sampler shutdown; call [`HardwareIntegration::shutdown`]
/// when the caller needs to await an in-flight metrics read.
pub struct HardwareIntegration {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}

impl HardwareIntegration {
    /// Create an integration using the five-second default sampler interval.
    ///
    /// Construction requires an active Tokio runtime because the shared
    /// sampler is owned by that runtime.
    pub fn new(metrics_provider: Arc<dyn MetricsProvider>) -> hass_core::Result<Self> {
        Self::with_config(metrics_provider, HardwareConfig::default())
    }

    /// Create an integration with an explicit interval and bounded event queue.
    pub fn with_config(
        metrics_provider: Arc<dyn MetricsProvider>,
        config: HardwareConfig,
    ) -> hass_core::Result<Self> {
        if config.sample_interval.is_zero() {
            return Err(hass_core::Error::InvalidInput(
                "hardware status sample interval must be greater than zero".to_owned(),
            ));
        }
        if config.status_channel_capacity == 0 {
            return Err(hass_core::Error::InvalidInput(
                "hardware status channel capacity must be greater than zero".to_owned(),
            ));
        }

        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            hass_core::Error::Service(format!(
                "hardware integration requires a Tokio runtime: {error}"
            ))
        })?;

        let (active_tx, active_rx) = watch::channel(ActiveState::default());
        let (stop_tx, stop_rx) = watch::channel(false);
        let (status_tx, _) = broadcast::channel(config.status_channel_capacity);
        let inner = Arc::new(Inner {
            state: Mutex::new(RegistryState {
                running: true,
                next_token: 1,
                generation: 0,
                providers: BTreeMap::new(),
                subscribers: BTreeSet::new(),
                status_tx: Some(status_tx),
                info_jobs: 0,
            }),
            active_tx,
            stop_tx,
            info_idle: Notify::new(),
            metrics_provider,
            config,
        });
        let worker = runtime.spawn(sampler_loop(Arc::clone(&inner), active_rx, stop_rx));

        Ok(Self {
            inner,
            worker: Some(worker),
        })
    }

    /// Obtain a weak, cloneable handle for provider registration and reads.
    pub fn manager(&self) -> HardwareManager {
        HardwareManager {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Stop accepting manager operations and await the sampler, including any
    /// blocking metrics read and hardware-info callbacks already admitted.
    pub async fn shutdown(&mut self) -> hass_core::Result<()> {
        self.inner.request_shutdown();
        let result = if let Some(worker) = self.worker.take() {
            worker.await.map_err(|error| {
                hass_core::Error::Service(format!("hardware system-status sampler failed: {error}"))
            })
        } else {
            Ok(())
        };
        self.inner.wait_for_info_jobs().await;
        result
    }
}

impl Drop for HardwareIntegration {
    fn drop(&mut self) {
        self.inner.request_shutdown();
    }
}

/// Weak, cloneable interface to a live hardware integration.
#[derive(Clone)]
pub struct HardwareManager {
    inner: Weak<Inner>,
}

impl HardwareManager {
    /// Register a provider for the lifetime of the returned guard.
    ///
    /// A second registration for the same domain replaces the first one. Each
    /// guard carries its registration token, so dropping a stale guard cannot
    /// remove a newer provider for that domain.
    pub fn register_provider(
        &self,
        domain: impl Into<String>,
        provider: Arc<dyn HardwareProvider>,
    ) -> hass_core::Result<ProviderGuard> {
        let inner = self.live_inner()?;
        let domain = domain.into();
        let mut state = lock_state(&inner);
        ensure_running(&state)?;

        let id = next_token(&mut state)?;
        state
            .providers
            .insert(domain.clone(), ProviderRegistration { id, provider });
        Ok(ProviderGuard {
            inner: Arc::downgrade(&inner),
            domain,
            id,
        })
    }

    /// Collect rows from currently registered providers.
    ///
    /// Provider `hass_core::Error` values and callback panics are logged and
    /// omitted, matching the per-provider failure boundary of the source API.
    /// If no provider is registered, the successful result is an empty vector.
    pub async fn hardware_info(&self) -> hass_core::Result<Vec<HardwareInfo>> {
        let inner = self.live_inner()?;
        let providers = {
            let mut state = lock_state(&inner);
            ensure_running(&state)?;
            let providers = state
                .providers
                .iter()
                .map(|(domain, entry)| (domain.clone(), Arc::clone(&entry.provider)))
                .collect::<Vec<_>>();
            state.info_jobs = state
                .info_jobs
                .checked_add(1)
                .ok_or_else(|| hass_core::Error::Service("too many hardware-info jobs".into()))?;
            providers
        };
        // The completion guard lives inside the blocking job. Cancelling the
        // caller's future cannot detach an untracked provider operation.
        let complete = InfoCompletion(inner);
        tokio::task::spawn_blocking(move || {
            let _complete = complete;
            let mut records = Vec::new();
            for (domain, provider) in providers {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| provider.hardware_info())) {
                Ok(Ok(mut provider_records)) => records.append(&mut provider_records),
                Ok(Err(error)) => {
                    warn!(integration = %domain, error = %error, "hardware provider failed; omitting its records");
                }
                Err(_) => {
                    warn!(integration = %domain, "hardware provider panicked; omitting its records");
                }
            }
            }
            records
        }).await.map_err(|error| hass_core::Error::Service(format!("hardware-info task did not complete: {error}")))
    }

    /// Subscribe to the shared system-status stream.
    ///
    /// The first event is emitted only after one full configured sampling
    /// interval. All subscriptions share one sampler and a bounded broadcast
    /// queue; [`SystemStatusSubscription::recv`] exposes lag as a Tokio
    /// `RecvError::Lagged` result.
    pub fn subscribe_system_status(&self) -> hass_core::Result<SystemStatusSubscription> {
        let inner = self.live_inner()?;
        let mut state = lock_state(&inner);
        ensure_running(&state)?;

        let id = next_token(&mut state)?;
        let receiver = state
            .status_tx
            .as_ref()
            .ok_or_else(stopped_error)?
            .subscribe();
        if state.subscribers.is_empty() {
            state.generation = state.generation.wrapping_add(1);
            inner.active_tx.send_replace(ActiveState {
                active: true,
                generation: state.generation,
            });
        }
        state.subscribers.insert(id);

        Ok(SystemStatusSubscription {
            inner: Arc::downgrade(&inner),
            id,
            receiver,
        })
    }

    fn live_inner(&self) -> hass_core::Result<Arc<Inner>> {
        let inner = self.inner.upgrade().ok_or_else(stopped_error)?;
        if !lock_state(&inner).running {
            return Err(stopped_error());
        }
        Ok(inner)
    }
}

/// RAII token that unregisters exactly the provider registration it created.
pub struct ProviderGuard {
    inner: Weak<Inner>,
    domain: String,
    id: u64,
}

impl Drop for ProviderGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = lock_state(&inner);
        if state
            .providers
            .get(&self.domain)
            .is_some_and(|registration| registration.id == self.id)
        {
            state.providers.remove(&self.domain);
        }
    }
}

/// RAII subscription to the shared system-status stream.
pub struct SystemStatusSubscription {
    inner: Weak<Inner>,
    id: u64,
    receiver: broadcast::Receiver<SystemStatus>,
}

impl SystemStatusSubscription {
    /// Wait for the next status event, or an observable lag/closed error.
    pub async fn recv(&mut self) -> std::result::Result<SystemStatus, broadcast::error::RecvError> {
        self.receiver.recv().await
    }

    /// Access the bounded Tokio receiver directly for `try_recv` and other
    /// receiver operations.
    pub fn receiver(&mut self) -> &mut broadcast::Receiver<SystemStatus> {
        &mut self.receiver
    }
}

impl Drop for SystemStatusSubscription {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = lock_state(&inner);
        if state.subscribers.remove(&self.id) && state.subscribers.is_empty() {
            state.generation = state.generation.wrapping_add(1);
            inner.active_tx.send_replace(ActiveState {
                active: false,
                generation: state.generation,
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ActiveState {
    active: bool,
    generation: u64,
}

struct ProviderRegistration {
    id: u64,
    provider: Arc<dyn HardwareProvider>,
}

struct RegistryState {
    running: bool,
    next_token: u64,
    generation: u64,
    providers: BTreeMap<String, ProviderRegistration>,
    subscribers: BTreeSet<u64>,
    status_tx: Option<broadcast::Sender<SystemStatus>>,
    info_jobs: usize,
}

struct Inner {
    state: Mutex<RegistryState>,
    active_tx: watch::Sender<ActiveState>,
    stop_tx: watch::Sender<bool>,
    info_idle: Notify,
    metrics_provider: Arc<dyn MetricsProvider>,
    config: HardwareConfig,
}

impl Inner {
    fn request_shutdown(&self) {
        let mut state = lock_state(self);
        if state.running {
            state.running = false;
            state.providers.clear();
            state.subscribers.clear();
            state.status_tx.take();
            state.generation = state.generation.wrapping_add(1);
            self.active_tx.send_replace(ActiveState {
                active: false,
                generation: state.generation,
            });
            self.stop_tx.send_replace(true);
        }
    }

    fn has_active_generation(&self, generation: u64) -> bool {
        let state = lock_state(self);
        state.running && !state.subscribers.is_empty() && state.generation == generation
    }

    async fn wait_for_info_jobs(&self) {
        loop {
            let notified = self.info_idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if lock_state(self).info_jobs == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct InfoCompletion(Arc<Inner>);

impl Drop for InfoCompletion {
    fn drop(&mut self) {
        lock_state(&self.0).info_jobs -= 1;
        self.0.info_idle.notify_waiters();
    }
}

async fn sampler_loop(
    inner: Arc<Inner>,
    mut active_rx: watch::Receiver<ActiveState>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut next_tick: Option<(u64, tokio::time::Instant)> = None;
    loop {
        if *stop_rx.borrow() {
            return;
        }

        let active = *active_rx.borrow_and_update();
        if !active.active {
            next_tick = None;
            tokio::select! {
                changed = active_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        return;
                    }
                }
            }
            continue;
        }

        let deadline = next_tick
            .filter(|(generation, _)| *generation == active.generation)
            .map_or_else(
                || tokio::time::Instant::now() + inner.config.sample_interval,
                |(_, deadline)| deadline,
            );
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {}
            changed = active_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    return;
                }
                continue;
            }
        }

        if *stop_rx.borrow() || !inner.has_active_generation(active.generation) {
            continue;
        }

        // The pinned interval helper schedules its next tick before calling
        // the status callback. Read duration must not add another full period.
        next_tick = Some((
            active.generation,
            tokio::time::Instant::now() + inner.config.sample_interval,
        ));

        let metrics_provider = Arc::clone(&inner.metrics_provider);
        let result = tokio::task::spawn_blocking(move || metrics_provider.read()).await;

        // The generation changes on last-unsubscribe and on the next first
        // subscription. An in-flight read from an earlier subscriber set must
        // not create an immediate event for a later set.
        if *stop_rx.borrow() || !inner.has_active_generation(active.generation) {
            if *stop_rx.borrow() {
                return;
            }
            continue;
        }

        match result {
            Ok(Ok(metrics)) => match SystemStatus::from_metrics(metrics) {
                Ok(status) => {
                    let state = lock_state(&inner);
                    if state.running
                        && !state.subscribers.is_empty()
                        && state.generation == active.generation
                        && let Some(sender) = &state.status_tx
                    {
                        let _ = sender.send(status);
                    }
                }
                Err(error) => {
                    warn!(error = %error, "invalid system metrics sample; omitting status event");
                }
            },
            Ok(Err(error)) => {
                warn!(error = %error, "system metrics provider failed; omitting status event");
            }
            Err(error) if error.is_panic() => {
                warn!(error = %error, "system metrics provider panicked; omitting status event");
            }
            Err(error) => {
                warn!(error = %error, "system metrics task did not complete; omitting status event");
            }
        }
    }
}

fn next_token(state: &mut RegistryState) -> hass_core::Result<u64> {
    let id = state.next_token;
    state.next_token = state
        .next_token
        .checked_add(1)
        .ok_or_else(|| hass_core::Error::Service("hardware token space exhausted".to_owned()))?;
    Ok(id)
}

fn ensure_running(state: &RegistryState) -> hass_core::Result<()> {
    if state.running {
        Ok(())
    } else {
        Err(stopped_error())
    }
}

fn stopped_error() -> hass_core::Error {
    hass_core::Error::Service("hardware integration has stopped".to_owned())
}

fn lock_state(inner: &Inner) -> MutexGuard<'_, RegistryState> {
    inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
