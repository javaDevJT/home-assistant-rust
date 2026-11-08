//! Cancellation-safe ownership for service handlers dispatched through Tokio.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::{Error, Result, ServiceResult};

#[derive(Clone)]
pub(crate) struct TaskOutcome {
    result: Result<ServiceResult>,
    join_error: Option<String>,
}

/// A cloneable explicit cancellation capability for one owned async service call.
/// Dropping this handle does not cancel the call.
#[derive(Clone)]
pub struct ServiceCallCancellationHandle {
    abort: AbortHandle,
    cancellation_requested: Arc<AtomicBool>,
}

impl ServiceCallCancellationHandle {
    /// Request cancellation of the owned call. Its waiter receives an error;
    /// an intentional abort is not reported as a task failure at shutdown.
    pub fn cancel(&self) {
        self.cancellation_requested.store(true, Ordering::Release);
        self.abort.abort();
    }
}

/// A result waiter for a tracker-owned async service call.
///
/// Dropping this value detaches the waiter. The runtime continues to own and
/// join the service task until it completes or is explicitly canceled.
pub struct OwnedServiceCall {
    completion: watch::Receiver<Option<TaskOutcome>>,
    cancellation: ServiceCallCancellationHandle,
}

impl OwnedServiceCall {
    /// Wait for the owned call's result. The same completed result may be read
    /// again if this method is called more than once.
    pub async fn result(&mut self) -> Result<ServiceResult> {
        wait_for_result(&mut self.completion).await
    }

    /// Return a cloneable handle that can explicitly cancel this call.
    pub fn cancellation_handle(&self) -> ServiceCallCancellationHandle {
        self.cancellation.clone()
    }
}

struct TaskState {
    admission_closed: bool,
    shutdown_started: bool,
    next_id: u64,
    tasks: BTreeMap<u64, watch::Sender<Option<TaskOutcome>>>,
    failures: BTreeMap<u64, String>,
}

/// Retains service-handler joins after request futures or result waiters drop.
///
/// The tracker is created with `Runtime`, but it does not start Tokio work
/// until an async service call admits a synchronous or explicitly owned async
/// handler.
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
                failures: BTreeMap::new(),
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

        let id = next_task_id(&mut state);
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
            tracker.finish(id, &outcome);
            completion.send_replace(Some(outcome));
        });
        Ok(receiver)
    }

    /// Admit and retain an async service task. Its join result is always
    /// observed by the reaper, even if its result waiter is dropped.
    pub(crate) fn spawn_async<F>(
        self: &Arc<Self>,
        handle: &Handle,
        work: F,
    ) -> Result<OwnedServiceCall>
    where
        F: Future<Output = Result<ServiceResult>> + Send + 'static,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.admission_closed {
            return Err(Error::Service(
                "service task admission is closed".to_owned(),
            ));
        }

        let id = next_task_id(&mut state);
        let (completion, receiver) = watch::channel(None);
        let previous = state.tasks.insert(id, completion.clone());
        debug_assert!(
            previous.is_none(),
            "service task ID wrapped onto a live task"
        );

        let cancellation_requested = Arc::new(AtomicBool::new(false));
        let task = handle.spawn(work);
        let cancellation = ServiceCallCancellationHandle {
            abort: task.abort_handle(),
            cancellation_requested: Arc::clone(&cancellation_requested),
        };
        let tracker = Arc::clone(self);
        handle.spawn(async move {
            let outcome = match task.await {
                Ok(result) => TaskOutcome {
                    result,
                    join_error: None,
                },
                Err(error)
                    if error.is_cancelled() && cancellation_requested.load(Ordering::Acquire) =>
                {
                    TaskOutcome {
                        result: Err(Error::Service(
                            "owned async service call was canceled".to_owned(),
                        )),
                        join_error: None,
                    }
                }
                Err(error) => {
                    let message = format!("async service handler task failed: {error}");
                    TaskOutcome {
                        result: Err(Error::Service(message.clone())),
                        join_error: Some(message),
                    }
                }
            };
            tracker.finish(id, &outcome);
            completion.send_replace(Some(outcome));
        });
        Ok(OwnedServiceCall {
            completion: receiver,
            cancellation,
        })
    }

    /// Close task admission, report completed unexpected join failures, and
    /// drain every retained service-task join. The drain runs independently of
    /// this waiter, so canceling one shutdown caller does not abandon work or
    /// change the outcome observed by later callers.
    pub(crate) async fn shutdown(self: &Arc<Self>) -> Result<()> {
        let handle = Handle::try_current().map_err(|_| {
            Error::Service("service task shutdown requires an active Tokio runtime".to_owned())
        })?;
        let mut shutdown = self.shutdown.subscribe();
        let drain = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.shutdown_started {
                None
            } else {
                state.admission_closed = true;
                state.shutdown_started = true;
                let failures = std::mem::take(&mut state.failures);
                let records = state
                    .tasks
                    .iter()
                    .map(|(id, completion)| (*id, completion.subscribe()))
                    .collect::<Vec<_>>();
                Some((failures, records))
            }
        };

        if let Some((mut failures, records)) = drain {
            if records.is_empty() {
                self.complete_shutdown(shutdown_result(failures));
            } else {
                let tracker = Arc::clone(self);
                handle.spawn(async move {
                    for (id, mut completion) in records {
                        if let Some(outcome) = wait_for_outcome(&mut completion).await
                            && let Some(message) = outcome.join_error
                        {
                            failures.entry(id).or_insert(message);
                        }
                    }
                    tracker.complete_shutdown(shutdown_result(failures));
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

    fn finish(&self, id: u64, outcome: &TaskOutcome) {
        let failure = outcome.join_error.clone();
        let (removed, replaced_failure) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let removed = state.tasks.remove(&id);
            let replaced_failure = if !state.shutdown_started {
                failure.and_then(|message| state.failures.insert(id, message))
            } else {
                None
            };
            (removed, replaced_failure)
        };
        // Sender and prior failure destruction happen outside the state lock.
        drop(removed);
        drop(replaced_failure);
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

fn next_task_id(state: &mut TaskState) -> u64 {
    loop {
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        if !state.tasks.contains_key(&id) && !state.failures.contains_key(&id) {
            return id;
        }
    }
}

fn shutdown_result(mut failures: BTreeMap<u64, String>) -> Result<()> {
    failures
        .pop_first()
        .map(|(_, message)| Error::Service(message))
        .map_or(Ok(()), Err)
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
