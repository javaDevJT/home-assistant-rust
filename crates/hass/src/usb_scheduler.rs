use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use hass_usb::{ScheduledTask, TimerScheduler};
use tokio::{
    runtime::Handle,
    sync::{Notify, watch},
};

/// Tokio-backed timer scheduler for the synchronous USB scanner callbacks.
///
/// Timer callbacks are always admitted to Tokio's blocking pool. The scheduler
/// keeps every admitted job until its timer has exited or its blocking callback
/// has been joined, so [`Self::shutdown`] is a complete lifecycle barrier.
#[derive(Clone)]
pub struct UsbScheduler {
    inner: Arc<Inner>,
    _owner: Arc<Owner>,
}

struct Owner {
    inner: std::sync::Weak<Inner>,
}

struct Inner {
    runtime: Handle,
    state: Mutex<SchedulerState>,
}

struct SchedulerState {
    accepting: bool,
    next_id: u64,
    jobs: HashMap<u64, Arc<Job>>,
}

struct Job {
    state: Mutex<JobState>,
    cancelled: Notify,
    completed: watch::Sender<bool>,
}

struct JobState {
    phase: JobPhase,
    callback: Option<Box<dyn FnOnce() + Send + 'static>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobPhase {
    Pending,
    Running,
    Cancelled,
    Complete,
}

struct JobHandle {
    job: Arc<Job>,
}

struct RejectedTask;

impl UsbScheduler {
    /// Create a scheduler whose timers and callbacks run on `runtime`.
    pub fn new(runtime: Handle) -> Self {
        let inner = Arc::new(Inner {
            runtime,
            state: Mutex::new(SchedulerState {
                accepting: true,
                next_id: 0,
                jobs: HashMap::new(),
            }),
        });
        let owner = Arc::new(Owner {
            inner: Arc::downgrade(&inner),
        });
        Self {
            inner,
            _owner: owner,
        }
    }

    /// Stop admitting timers, cancel pending callbacks, and wait for all jobs
    /// already admitted. A callback that has begun is allowed to finish.
    pub async fn shutdown(&self) {
        let jobs = self.inner.stop_admission();
        for job in jobs {
            job.wait_until_complete().await;
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            // Drop cannot await, but it still prevents new work and promptly
            // wakes every pending timer. Running callbacks retain their job
            // task until the blocking JoinHandle completes.
            inner.stop_admission();
        }
    }
}

impl TimerScheduler for UsbScheduler {
    fn schedule(
        &self,
        delay: Duration,
        callback: Box<dyn FnOnce() + Send + 'static>,
    ) -> Box<dyn ScheduledTask> {
        let (id, job) = {
            let mut state = lock(&self.inner.state);
            if !state.accepting {
                return Box::new(RejectedTask);
            }

            state.next_id = state.next_id.wrapping_add(1);
            let id = state.next_id;
            let job = Arc::new(Job::new(callback));
            state.jobs.insert(id, job.clone());
            (id, job)
        };

        let inner = self.inner.clone();
        let timer_job = job.clone();
        self.inner
            .runtime
            .spawn(async move { run_job(inner, id, timer_job, delay).await });

        Box::new(JobHandle { job })
    }
}

impl ScheduledTask for JobHandle {
    /// This method is deliberately synchronous and non-blocking. In
    /// particular, a callback may cancel its own handle without waiting on
    /// itself or blocking a Tokio worker.
    fn cancel(&self) {
        self.job.cancel();
    }
}

impl ScheduledTask for RejectedTask {
    fn cancel(&self) {}
}

impl Job {
    fn new(callback: Box<dyn FnOnce() + Send + 'static>) -> Self {
        let (completed, _) = watch::channel(false);
        Self {
            state: Mutex::new(JobState {
                phase: JobPhase::Pending,
                callback: Some(callback),
            }),
            cancelled: Notify::new(),
            completed,
        }
    }

    fn cancel(&self) {
        let (was_pending, callback) = {
            let mut state = lock(&self.state);
            if state.phase == JobPhase::Pending {
                state.phase = JobPhase::Cancelled;
                (true, state.callback.take())
            } else {
                (false, None)
            }
        };

        // Drop the callback outside the mutex in case its captures have
        // nontrivial destructors.
        drop(callback);
        if was_pending {
            self.cancelled.notify_one();
        }
    }

    fn admit(&self) -> Option<Box<dyn FnOnce() + Send + 'static>> {
        let mut state = lock(&self.state);
        if state.phase != JobPhase::Pending {
            return None;
        }
        state.phase = JobPhase::Running;
        state.callback.take()
    }

    fn mark_complete(&self) {
        let callback = {
            let mut state = lock(&self.state);
            state.phase = JobPhase::Complete;
            state.callback.take()
        };
        drop(callback);
        self.completed.send_replace(true);
    }

    async fn wait_until_complete(&self) {
        let mut completed = self.completed.subscribe();
        while !*completed.borrow_and_update() {
            if completed.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Inner {
    fn stop_admission(&self) -> Vec<Arc<Job>> {
        let jobs = {
            let mut state = lock(&self.state);
            state.accepting = false;
            state.jobs.values().cloned().collect::<Vec<_>>()
        };
        for job in &jobs {
            job.cancel();
        }
        jobs
    }
}

async fn run_job(inner: Arc<Inner>, id: u64, job: Arc<Job>, delay: Duration) {
    let cancelled = job.cancelled.notified();
    tokio::pin!(cancelled);

    tokio::select! {
        _ = tokio::time::sleep(delay) => {
            if let Some(callback) = job.admit() {
                // Await the blocking JoinHandle before completing/reaping this
                // job. Shutdown therefore joins callbacks rather than merely
                // waiting for the timer task to hand them off.
                let callback = inner.runtime.spawn_blocking(callback);
                let _ = callback.await;
            }
        }
        _ = &mut cancelled => {}
    }

    {
        let mut state = lock(&inner.state);
        state.jobs.remove(&id);
    }
    job.mark_complete();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Condvar,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_pending_timer_never_runs_its_callback() {
        let scheduler = UsbScheduler::new(Handle::current());
        let called = Arc::new(AtomicUsize::new(0));
        let callback_called = called.clone();

        let task = scheduler.schedule(
            Duration::from_secs(60),
            Box::new(move || {
                callback_called.fetch_add(1, Ordering::SeqCst);
            }),
        );
        task.cancel();
        scheduler.shutdown().await;

        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert!(lock(&scheduler.inner.state).jobs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_last_scheduler_owner_cancels_pending_timers() {
        let scheduler = UsbScheduler::new(Handle::current());
        let called = Arc::new(AtomicUsize::new(0));
        let callback_called = called.clone();

        let task = scheduler.schedule(
            Duration::from_millis(30),
            Box::new(move || {
                callback_called.fetch_add(1, Ordering::SeqCst);
            }),
        );
        drop(scheduler);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
        drop(task);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn callbacks_run_on_the_blocking_pool() {
        let scheduler = UsbScheduler::new(Handle::current());
        let runtime_thread = thread::current().id();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();

        scheduler.schedule(
            Duration::ZERO,
            Box::new(move || {
                let callback_thread = thread::current().id();
                let _ = finished_tx.send(callback_thread != runtime_thread);
            }),
        );

        assert!(finished_rx.await.expect("callback should finish"));
        scheduler.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_waits_for_running_callback_and_self_cancel_is_nonblocking() {
        let scheduler = UsbScheduler::new(Handle::current());
        let callback_scheduler = scheduler.clone();
        let self_handle = Arc::new((Mutex::new(None::<Arc<dyn ScheduledTask>>), Condvar::new()));
        let callback_handle = self_handle.clone();
        let callback_started = Arc::new(Notify::new());
        let callback_started_in_job = callback_started.clone();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let callback_gate = gate.clone();
        let callback_finished = Arc::new(AtomicUsize::new(0));
        let callback_finished_in_job = callback_finished.clone();

        let task = callback_scheduler.schedule(
            Duration::ZERO,
            Box::new(move || {
                let handle = {
                    let (slot, ready) = &*callback_handle;
                    let mut slot = lock(slot);
                    while slot.is_none() {
                        slot = ready
                            .wait(slot)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    slot.as_ref().expect("handle installed").clone()
                };

                handle.cancel();
                callback_started_in_job.notify_one();

                let (released, ready) = &*callback_gate;
                let mut released = lock(released);
                while !*released {
                    released = ready
                        .wait(released)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                callback_finished_in_job.fetch_add(1, Ordering::SeqCst);
            }),
        );

        let task: Arc<dyn ScheduledTask> = Arc::from(task);
        {
            let (slot, ready) = &*self_handle;
            *lock(slot) = Some(task);
            ready.notify_all();
        }
        callback_started.notified().await;

        let (shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel();
        let shutdown_scheduler = scheduler.clone();
        let mut shutdown = tokio::spawn(async move {
            let _ = shutdown_started_tx.send(());
            shutdown_scheduler.shutdown().await;
        });
        shutdown_started_rx
            .await
            .expect("shutdown task should start");
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
                .await
                .is_err()
        );
        assert_eq!(callback_finished.load(Ordering::SeqCst), 0);

        let (released, ready) = &*gate;
        *lock(released) = true;
        ready.notify_all();
        shutdown.await.expect("shutdown should join callback");

        assert_eq!(callback_finished.load(Ordering::SeqCst), 1);
        assert!(lock(&scheduler.inner.state).jobs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recursive_scheduling_is_supported_and_completed_jobs_are_reaped() {
        const CALLBACKS: usize = 12;

        let scheduler = UsbScheduler::new(Handle::current());
        let completed = Arc::new(tokio::sync::Semaphore::new(0));

        fn schedule_next(
            scheduler: UsbScheduler,
            remaining: usize,
            completed: Arc<tokio::sync::Semaphore>,
        ) {
            let next_scheduler = scheduler.clone();
            scheduler.schedule(
                Duration::ZERO,
                Box::new(move || {
                    if remaining > 1 {
                        schedule_next(next_scheduler, remaining - 1, completed.clone());
                    }
                    completed.add_permits(1);
                }),
            );
        }

        schedule_next(scheduler.clone(), CALLBACKS, completed.clone());
        let permits = completed
            .acquire_many(CALLBACKS as u32)
            .await
            .expect("recursive callbacks should complete");
        permits.forget();

        scheduler.shutdown().await;

        assert!(lock(&scheduler.inner.state).jobs.is_empty());
    }
}
