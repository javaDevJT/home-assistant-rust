//! Cancellation-safe ownership for synchronous service handlers dispatched
//! through Tokio's shared blocking pool.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::{Error, Result, ServiceResult};

#[derive(Clone)]
pub(crate) struct TaskOutcome {
    result: Result<ServiceResult>,
    join_error: Option<String>,
}

struct TaskState {
    admission_closed: bool,
    shutdown_started: bool,
    next_id: u64,
    tasks: BTreeMap<u64, watch::Sender<Option<TaskOutcome>>>,
}

/// Retains blocking-handler joins after request futures are canceled.
///
/// The tracker is created with `Runtime`, but it does not start Tokio work
/// until an async service call admits its first synchronous handler.
pub(crate) struct ServiceTaskTracker {
    state: Mutex<TaskState>,
    shutdown: watch::Sender<Option<Result<()>>>,
}

impl ServiceTaskTracker {
    pub(crate) fn new() -> Self {
        let (shutdown, _) = watch::channel(None);
        Self {
            state: Mutex::new(TaskState {
                admission_closed: false,
                shutdown_started: false,
                next_id: 1,
                tasks: BTreeMap::new(),
            }),
            shutdown,
        }
    }

    /// Admit and retain a blocking task. Its join result is always observed by
    /// the reaper, even if the request's receiver is dropped.
    pub(crate) fn spawn_blocking(
        self: &Arc<Self>,
        handle: &Handle,
        work: impl FnOnce() -> Result<ServiceResult> + Send + 'static,
    ) -> Result<watch::Receiver<Option<TaskOutcome>>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.admission_closed {
            return Err(Error::Service(
                "service task admission is closed".to_owned(),
            ));
        }

        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let (completion, receiver) = watch::channel(None);
        let previous = state.tasks.insert(id, completion.clone());
        debug_assert!(
            previous.is_none(),
            "service task ID wrapped onto a live task"
        );

        let blocking = handle.spawn_blocking(work);
        let tracker = Arc::clone(self);
        handle.spawn(async move {
            let outcome = match blocking.await {
                Ok(result) => TaskOutcome {
                    result,
                    join_error: None,
                },
                Err(error) => {
                    let message = format!("synchronous service handler task failed: {error}");
                    TaskOutcome {
                        result: Err(Error::Service(message.clone())),
                        join_error: Some(message),
                    }
                }
            };
            completion.send_replace(Some(outcome));
            tracker.finish(id);
        });
        Ok(receiver)
    }

    /// Close task admission and drain every retained blocking join. The drain
    /// runs independently of this waiter, so canceling one shutdown caller
    /// does not abandon work or change the outcome observed by later callers.
    pub(crate) async fn shutdown(self: &Arc<Self>) -> Result<()> {
        let handle = Handle::try_current().map_err(|_| {
            Error::Service("service task shutdown requires an active Tokio runtime".to_owned())
        })?;
        let mut shutdown = self.shutdown.subscribe();
        let records = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutdown_started {
                None
            } else {
                state.admission_closed = true;
                state.shutdown_started = true;
                Some(
                    state
                        .tasks
                        .values()
                        .map(|completion| completion.subscribe())
                        .collect::<Vec<_>>(),
                )
            }
        };

        if let Some(records) = records {
            if records.is_empty() {
                self.complete_shutdown(Ok(()));
            } else {
                let tracker = Arc::clone(self);
                handle.spawn(async move {
                    let mut first_join_error = None;
                    for mut completion in records {
                        if let Some(outcome) = wait_for_outcome(&mut completion).await
                            && first_join_error.is_none()
                        {
                            first_join_error = outcome.join_error;
                        }
                    }
                    let result = first_join_error.map(Error::Service).map_or(Ok(()), Err);
                    tracker.complete_shutdown(result);
                });
            }
        }

        loop {
            if let Some(result) = shutdown.borrow_and_update().clone() {
                return result;
            }
            shutdown.changed().await.map_err(|_| {
                Error::Service("service task shutdown ended without a result".to_owned())
            })?;
        }
    }

    fn finish(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .remove(&id);
    }

    fn complete_shutdown(&self, result: Result<()>) {
        self.shutdown.send_replace(Some(result));
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .len()
    }

    #[cfg(test)]
    pub(crate) fn admission_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admission_closed
    }
}

pub(crate) async fn wait_for_result(
    completion: &mut watch::Receiver<Option<TaskOutcome>>,
) -> Result<ServiceResult> {
    match wait_for_outcome(completion).await {
        Some(outcome) => outcome.result,
        None => Err(Error::Service(
            "synchronous service task ended without a result".to_owned(),
        )),
    }
}

async fn wait_for_outcome(
    completion: &mut watch::Receiver<Option<TaskOutcome>>,
) -> Option<TaskOutcome> {
    loop {
        if let Some(outcome) = completion.borrow_and_update().clone() {
            return Some(outcome);
        }
        if completion.changed().await.is_err() {
            return completion.borrow().clone();
        }
    }
}
