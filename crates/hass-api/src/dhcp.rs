//! WebSocket transport for live DHCP discovery updates.
//!
//! The listener is registered before the subscription result is sent. Cache
//! replay starts only after that result, and live updates stay buffered until
//! the WebSocket owner reads them.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use hass_core::Result;
use hass_dhcp::{DhcpAddressData, DhcpManager, DhcpUpdate, DhcpUpdateGuard, format_mac};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

const STREAM_CHANNEL_CAPACITY: usize = 64;
const SNAPSHOT_YIELD_INTERVAL: usize = 128;

/// Owns one DHCP update listener, a bounded event queue, and its cache
/// snapshot task for one WebSocket subscription.
///
/// `prepare` registers the exact listener before the caller sends the ACK.
/// The caller starts the snapshot after sending the ACK. Queue overflow ends
/// the stream and leaves a visible error for the WebSocket owner.
pub(crate) struct DhcpStream {
    receiver: mpsc::Receiver<Value>,
    sender: mpsc::Sender<Value>,
    id: u64,
    manager: DhcpManager,
    update_guard: Option<DhcpUpdateGuard>,
    snapshot_task: Option<JoinHandle<()>>,
    snapshot_started: bool,
    active: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl DhcpStream {
    /// Register the exact DHCP update listener and prepare a bounded queue.
    ///
    /// This function is synchronous so the caller can guarantee that live
    /// observations are observed before it sends the subscription result.
    pub(crate) fn prepare(manager: DhcpManager, id: u64) -> Result<Self> {
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let active = Arc::new(AtomicBool::new(true));
        let error = Arc::new(Mutex::new(None));

        let update_sender = sender.clone();
        let update_active = Arc::clone(&active);
        let update_error = Arc::clone(&error);
        let update_guard = manager.register_update_listener(move |update: DhcpUpdate| {
            if !update_active.load(Ordering::Acquire) {
                return;
            }
            enqueue_event(
                &update_sender,
                id,
                add_event(&update.mac, &update.data),
                &update_active,
                &update_error,
            );
        })?;

        Ok(Self {
            receiver,
            sender,
            id,
            manager,
            update_guard: Some(update_guard),
            snapshot_task: None,
            snapshot_started: false,
            active,
            error,
        })
    }

    /// Start the cache snapshot after the subscription result has been sent.
    ///
    /// The task is owned by the stream so unsubscribe and socket teardown can
    /// cancel and join it. It sends one add-list frame even when the cache is
    /// empty, matching Home Assistant's callback behavior.
    pub(crate) fn start_snapshot(&mut self) {
        if self.snapshot_started {
            return;
        }
        self.snapshot_started = true;

        let manager = self.manager.clone();
        let sender = self.sender.clone();
        let active = Arc::clone(&self.active);
        let error = Arc::clone(&self.error);
        let id = self.id;

        self.snapshot_task = Some(tokio::spawn(async move {
            let rows = match manager.address_data_snapshot() {
                Ok(rows) => rows,
                Err(snapshot_error) => {
                    report_error(
                        &active,
                        &error,
                        format!("DHCP snapshot failed: {snapshot_error}"),
                    );
                    return;
                }
            };

            let mut additions = Vec::with_capacity(rows.len());
            for (index, (mac, data)) in rows.into_iter().enumerate() {
                if !active.load(Ordering::Acquire) {
                    return;
                }
                additions.push(add_row(&mac, &data));
                if index % SNAPSHOT_YIELD_INTERVAL == SNAPSHOT_YIELD_INTERVAL - 1 {
                    tokio::task::yield_now().await;
                }
            }

            if active.load(Ordering::Acquire) {
                enqueue_event(&sender, id, json!({"add": additions}), &active, &error);
            }
        }));
    }

    /// Receive one complete event frame without waiting.
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

    /// Return true once the inactive subscription and its queued frames drain.
    pub(crate) fn is_finished(&self) -> bool {
        !self.active.load(Ordering::Acquire)
            && self.receiver.is_empty()
            && self
                .snapshot_task
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }

    /// Join an inactive stream and release its exact update listener.
    pub(crate) async fn join(mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listener();
        if let Some(task) = self.snapshot_task.take() {
            let _ = task.await;
        }
    }

    /// Cancel the snapshot, release the listener, and wait for owned work.
    pub(crate) async fn cancel(mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listener();
        if let Some(task) = self.snapshot_task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    fn release_listener(&mut self) {
        self.update_guard.take();
    }
}

impl Drop for DhcpStream {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.release_listener();
        if let Some(task) = &self.snapshot_task {
            task.abort();
        }
    }
}

fn add_event(mac: &str, data: &DhcpAddressData) -> Value {
    json!({"add": [add_row(mac, data)]})
}

fn add_row(mac: &str, data: &DhcpAddressData) -> Value {
    // Home Assistant formats the cache key, then uppercases the formatted MAC
    // for this WebSocket API. Use raw cache data for hostname and IP; discovery
    // service-info lowercases hostnames and is not the source for these rows.
    json!({
        "mac_address": format_mac(mac).to_uppercase(),
        "hostname": data.hostname,
        "ip_address": data.ip,
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
                "DHCP event queue overflowed; subscription removed".to_owned(),
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
