//! WebSocket transport for live SSDP discovery callbacks.
//!
//! Each subscription registers off the WebSocket receive loop because cached
//! SSDP replay may fetch a device description. Callback frames use a bounded
//! queue so a slow client cannot retain unbounded discovery data.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use hass_ssdp::{MatchDict, SsdpCallback, SsdpChange, SsdpManager, SsdpServiceInfo};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Owns one SSDP callback registration and its bounded WebSocket frame queue.
///
/// The stream is inserted only after the subscription ACK is sent. Explicit
/// cleanup signals cancellation and joins the registration task, which drops
/// its callback guard before returning.
pub(crate) struct SsdpStream {
    receiver: mpsc::Receiver<Value>,
    task: Option<JoinHandle<()>>,
    cancel: Option<oneshot::Sender<()>>,
    active: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl SsdpStream {
    /// Start cached replay and live callback registration in an owned task.
    pub(crate) fn start(manager: SsdpManager, id: u64) -> Self {
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (cancel, mut cancel_receiver) = oneshot::channel();
        let active = Arc::new(AtomicBool::new(true));
        let error = Arc::new(Mutex::new(None));
        let notify = Arc::new(Notify::new());

        let callback_active = Arc::clone(&active);
        let callback_error = Arc::clone(&error);
        let callback_notify = Arc::clone(&notify);
        let callback_sender = sender.clone();
        let callback: SsdpCallback = Arc::new(move |info: &SsdpServiceInfo, change: SsdpChange| {
            if !callback_active.load(Ordering::Acquire) {
                return Ok(());
            }

            let event = match info.websocket_event(change) {
                Ok(event) => event,
                Err(error) => {
                    report_error(
                        &callback_active,
                        &callback_error,
                        &callback_notify,
                        error.to_string(),
                    );
                    return Err(error);
                }
            };
            let frame = json!({"id": id, "type": "event", "event": event});
            match callback_sender.try_send(frame) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    report_error(
                        &callback_active,
                        &callback_error,
                        &callback_notify,
                        "SSDP event queue overflowed; subscription removed".into(),
                    );
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Ok(()),
            }
        });

        let task_active = Arc::clone(&active);
        let task_error = Arc::clone(&error);
        let task_notify = Arc::clone(&notify);
        let task = tokio::spawn(async move {
            let registration = manager.register_callback(MatchDict::new(), callback);
            tokio::pin!(registration);
            let guard = tokio::select! {
                biased;
                _ = &mut cancel_receiver => {
                    task_active.store(false, Ordering::Release);
                    return;
                }
                _ = task_notify.notified() => {
                    task_active.store(false, Ordering::Release);
                    return;
                }
                result = &mut registration => match result {
                    Ok(guard) => guard,
                    Err(error) => {
                        report_error(
                            &task_active,
                            &task_error,
                            &task_notify,
                            format!("SSDP subscription failed: {error}"),
                        );
                        task_active.store(false, Ordering::Release);
                        return;
                    }
                },
            };

            tokio::select! {
                biased;
                _ = &mut cancel_receiver => {}
                _ = task_notify.notified() => {}
                _ = sender.closed() => {}
            }
            task_active.store(false, Ordering::Release);
            drop(guard);
        });

        Self {
            receiver,
            task: Some(task),
            cancel: Some(cancel),
            active,
            error,
        }
    }

    /// Receive one complete event or error frame without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Value, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Number of event frames the stream can queue ahead of the socket.
    pub(crate) const fn capacity(&self) -> usize {
        STREAM_CHANNEL_CAPACITY
    }

    /// Return the first registration, serialization, or overflow error.
    pub(crate) fn take_error(&self) -> Option<String> {
        lock(&self.error).take()
    }

    /// Return true once registration has ended and its queued frames are drained.
    pub(crate) fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished) && self.receiver.is_empty()
    }

    /// Join a stream whose callback task has completed.
    pub(crate) async fn join(mut self) {
        self.active.store(false, Ordering::Release);
        self.cancel.take();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Cancel cached replay or live delivery and wait for the callback guard to drop.
    pub(crate) async fn cancel(mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SsdpStream {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn report_error(
    active: &AtomicBool,
    error_slot: &Mutex<Option<String>>,
    notify: &Notify,
    message: String,
) {
    if !active.swap(false, Ordering::AcqRel) {
        return;
    }
    *lock(error_slot) = Some(message);
    notify.notify_one();
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
