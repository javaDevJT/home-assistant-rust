//! WebSocket transport for live Zeroconf discovery callbacks.
//!
//! Both manager listeners are registered synchronously before the caller sends
//! the subscription acknowledgement. The socket owner starts cache replay only
//! after sending that acknowledgement, while live callbacks remain registered
//! for the lifetime of the stream.
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::{StreamExt, stream};
use hass_core::Result;
use hass_zeroconf::{ListenerGuard, RawZeroconfServiceInfo, ZeroconfManager, ZeroconfProperties};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

const STREAM_CHANNEL_CAPACITY: usize = 64;
const SNAPSHOT_RESOLUTION_CONCURRENCY: usize = 16;

/// Owns the two Zeroconf listeners, a bounded event queue, and the cache
/// snapshot task associated with one WebSocket subscription.
///
/// `prepare` installs both listeners before returning. The caller must send the
/// WebSocket acknowledgement before calling `start_snapshot`; callbacks that
/// arrive during that gap remain queued but cannot be written to the socket by
/// this stream.
pub(crate) struct ZeroconfStream {
    receiver: mpsc::Receiver<Value>,
    sender: mpsc::Sender<Value>,
    id: u64,
    manager: ZeroconfManager,
    generated_types: Arc<HashSet<String>>,
    update_guard: Option<ListenerGuard>,
    removed_guard: Option<ListenerGuard>,
    snapshot_task: Option<JoinHandle<()>>,
    snapshot_started: bool,
    active: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl ZeroconfStream {
    /// Register both raw listeners and prepare the subscription queue.
    ///
    /// This function is deliberately synchronous so callers can guarantee the
    /// listeners exist before sending the command result. Only generated
    /// service types are included in add events and in the cache snapshot.
    pub(crate) fn prepare(manager: ZeroconfManager, id: u64) -> Result<Self> {
        let generated_types: Arc<HashSet<String>> =
            Arc::new(manager.service_types()?.into_iter().collect());
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let active = Arc::new(AtomicBool::new(true));
        let error = Arc::new(Mutex::new(None));

        let update_sender = sender.clone();
        let update_active = Arc::clone(&active);
        let update_error = Arc::clone(&error);
        let update_types = Arc::clone(&generated_types);
        let update_guard = manager.register_service_update_listener(move |raw| {
            if !update_types.contains(&raw.service_type) || !update_active.load(Ordering::Acquire) {
                return Ok(());
            }
            enqueue_event(
                &update_sender,
                id,
                add_event(raw),
                &update_active,
                &update_error,
            );
            Ok(())
        })?;

        let remove_sender = sender.clone();
        let remove_active = Arc::clone(&active);
        let remove_error = Arc::clone(&error);
        let removed_guard = manager.register_service_removed_listener(move |name| {
            if !remove_active.load(Ordering::Acquire) {
                return Ok(());
            }
            enqueue_event(
                &remove_sender,
                id,
                json!({"remove": [{"name": name}]}),
                &remove_active,
                &remove_error,
            );
            Ok(())
        })?;

        Ok(Self {
            receiver,
            sender,
            id,
            manager,
            generated_types,
            update_guard: Some(update_guard),
            removed_guard: Some(removed_guard),
            snapshot_task: None,
            snapshot_started: false,
            active,
            error,
        })
    }

    /// Start the generated-type cache snapshot after the acknowledgement.
    ///
    /// The task resolves incomplete PTR entries with bounded concurrency and
    /// remains owned by the stream so unsubscribe and socket teardown can
    /// cancel and join it. Live listeners are independent of this task and stay
    /// registered after the snapshot finishes.
    pub(crate) fn start_snapshot(&mut self) {
        if self.snapshot_started {
            return;
        }
        self.snapshot_started = true;

        let manager = self.manager.clone();
        let generated_types = Arc::clone(&self.generated_types);
        let sender = self.sender.clone();
        let active = Arc::clone(&self.active);
        let error = Arc::clone(&self.error);
        let id = self.id;

        self.snapshot_task = Some(tokio::spawn(async move {
            let names = match manager.snapshot_names() {
                Ok(names) => names,
                Err(snapshot_error) => {
                    report_error(
                        &active,
                        &error,
                        format!("Zeroconf snapshot failed: {snapshot_error}"),
                    );
                    return;
                }
            };

            let resolutions = stream::iter(names)
                .map(|(service_type, name)| {
                    let manager = manager.clone();
                    async move {
                        let record = match manager.get_cached(&service_type, &name) {
                            Ok(Some(record)) => Ok(Some(record)),
                            Ok(None) => manager.resolve(service_type.clone(), name.clone()).await,
                            Err(resolve_error) => Err(resolve_error),
                        };
                        (record, service_type, name)
                    }
                })
                .buffer_unordered(SNAPSHOT_RESOLUTION_CONCURRENCY);
            tokio::pin!(resolutions);

            while let Some((record, service_type, _name)) = resolutions.next().await {
                if !active.load(Ordering::Acquire) {
                    return;
                }
                let record = match record {
                    Ok(Some(record)) => record,
                    Ok(None) => continue,
                    Err(resolve_error) => {
                        report_error(
                            &active,
                            &error,
                            format!("Zeroconf snapshot resolution failed: {resolve_error}"),
                        );
                        return;
                    }
                };
                // PTR names come only from generated types. Check the resolved
                // type too, matching the update-listener filter if a backend
                // returns an inconsistent record.
                if !generated_types.contains(&service_type)
                    || !generated_types.contains(&record.service_type)
                {
                    continue;
                }
                if !enqueue_event(&sender, id, add_event(&record), &active, &error) {
                    return;
                }
            }
        }));
    }

    /// Receive one event frame without waiting.
    pub(crate) fn try_recv(&mut self) -> std::result::Result<Value, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Number of event frames the stream can queue ahead of the socket.
    pub(crate) const fn capacity(&self) -> usize {
        STREAM_CHANNEL_CAPACITY
    }

    /// Return and clear the first snapshot or queue error.
    pub(crate) fn take_error(&self) -> Option<String> {
        lock(&self.error).take()
    }

    /// Return true once the subscription is inactive and queued frames drain.
    pub(crate) fn is_finished(&self) -> bool {
        !self.active.load(Ordering::Acquire)
            && self.receiver.is_empty()
            && self
                .snapshot_task
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }

    /// Join a finished stream and synchronously release both listener guards.
    pub(crate) async fn join(mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listeners();
        if let Some(task) = self.snapshot_task.take() {
            let _ = task.await;
        }
    }

    /// Cancel the snapshot, release both listeners, and wait for owned work.
    pub(crate) async fn cancel(mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listeners();
        if let Some(task) = self.snapshot_task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    fn release_listeners(&mut self) {
        self.update_guard.take();
        self.removed_guard.take();
    }
}

impl Drop for ZeroconfStream {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listeners();
        if let Some(task) = &self.snapshot_task {
            task.abort();
        }
    }
}

fn add_event(raw: &RawZeroconfServiceInfo) -> Value {
    let properties = ZeroconfProperties::from_txt_entries(raw.txt_entries.clone());
    let ip_addresses = raw
        .ip_addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    json!({
        "add": [{
            "name": raw.name,
            "type": raw.service_type,
            "port": raw.port,
            "properties": properties,
            "ip_addresses": ip_addresses,
        }]
    })
}

fn enqueue_event(
    sender: &mpsc::Sender<Value>,
    id: u64,
    event: Value,
    active: &AtomicBool,
    error: &Mutex<Option<String>>,
) -> bool {
    if !active.load(Ordering::Acquire) {
        return false;
    }
    match sender.try_send(json!({"id": id, "type": "event", "event": event})) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            report_error(
                active,
                error,
                "Zeroconf event queue overflowed; subscription removed".to_owned(),
            );
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

fn report_error(active: &AtomicBool, error: &Mutex<Option<String>>, message: String) {
    if active.swap(false, Ordering::AcqRel) {
        *lock(error) = Some(message);
    }
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
