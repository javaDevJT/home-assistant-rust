//! WebSocket transport for the one-shot System Health information stream.
//!
//! Collection runs in an owned task so a slow provider cannot prevent the
//! socket from receiving commands, unsubscribe requests, or shutdown signals.

use hass_core::Error as CoreError;
use hass_system_health::SystemHealthManager;
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

const STREAM_CHANNEL_CAPACITY: usize = 4;

/// Owns one System Health request's bounded frame queue and collection task.
///
/// The WebSocket connection owns this value until it is unsubscribed or its
/// final frame has been drained. Dropping it is a best-effort cancellation;
/// explicit cleanup should use [`Self::cancel`] so the task is joined.
pub(crate) struct HealthStream {
    receiver: mpsc::Receiver<Value>,
    task: Option<JoinHandle<()>>,
}

impl HealthStream {
    /// Start collecting provider data without waiting for callbacks on the
    /// WebSocket receive loop.
    pub(crate) fn start(manager: SystemHealthManager, id: u64) -> Self {
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let task = tokio::spawn(async move {
            collect_stream(manager, id, sender).await;
        });
        Self {
            receiver,
            task: Some(task),
        }
    }

    /// Receive a complete WebSocket response frame without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Value, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Number of complete frames this stream can queue ahead of the socket.
    pub(crate) const fn capacity(&self) -> usize {
        STREAM_CHANNEL_CAPACITY
    }

    /// Return true once the task has ended and its queued frames are drained.
    pub(crate) fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished) && self.receiver.is_empty()
    }

    /// Join a stream that has naturally completed after all frames were read.
    pub(crate) async fn join(mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Cancel pending callbacks/deferred values and wait for task termination.
    pub(crate) async fn cancel(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for HealthStream {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn collect_stream(manager: SystemHealthManager, id: u64, sender: mpsc::Sender<Value>) {
    let mut session = match manager.collect().await {
        Ok(session) => session,
        Err(error) => {
            let _ = sender.send(collect_error_frame(id, error)).await;
            return;
        }
    };

    let initial = match serde_json::to_value(&session.initial) {
        Ok(initial) => initial,
        Err(_) => {
            let _ = sender.send(internal_error_frame(id)).await;
            return;
        }
    };

    // The source acknowledges only after every provider callback has finished.
    if sender.send(success_frame(id, Value::Null)).await.is_err() {
        return;
    }
    if sender
        .send(event_frame(
            id,
            json!({ "type": "initial", "data": initial }),
        ))
        .await
        .is_err()
    {
        return;
    }

    while let Some(update) = session.next_update().await {
        if sender
            .send(event_frame(id, update.to_transport_event()))
            .await
            .is_err()
        {
            return;
        }
    }

    let _ = sender
        .send(event_frame(id, json!({ "type": "finish" })))
        .await;
}

fn success_frame(id: u64, result: Value) -> Value {
    json!({
        "id": id,
        "type": "result",
        "success": true,
        "result": result,
    })
}

fn event_frame(id: u64, event: Value) -> Value {
    json!({
        "id": id,
        "type": "event",
        "event": event,
    })
}

fn collect_error_frame(id: u64, error: CoreError) -> Value {
    match error {
        CoreError::NotFound(_) => json!({
            "id": id,
            "type": "result",
            "success": false,
            "error": {
                "code": "not_found",
                "message": "System Health manager is unavailable",
            },
        }),
        _ => internal_error_frame(id),
    }
}

fn internal_error_frame(id: u64) -> Value {
    json!({
        "id": id,
        "type": "result",
        "success": false,
        "error": {
            "code": "unknown_error",
            "message": "Internal server error",
        },
    })
}
