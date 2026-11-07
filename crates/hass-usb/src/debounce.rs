use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use crate::manager::{DebounceRequest, UsbError};

/// Cancellation handle returned by the host's timer implementation.
pub trait ScheduledTask: Send + Sync {
    fn cancel(&self);
}

/// Timer seam for the app runtime. The SDK owns and cancels every returned
/// task; test schedulers can advance time without sleeping or touching a host.
pub trait TimerScheduler: Send + Sync {
    fn schedule(
        &self,
        delay: Duration,
        callback: Box<dyn FnOnce() + Send + 'static>,
    ) -> Box<dyn ScheduledTask>;
}

pub(crate) struct Debouncer {
    self_ref: Weak<Self>,
    scheduler: Arc<dyn TimerScheduler>,
    cooldown: Duration,
    immediate: bool,
    job: Arc<dyn Fn() -> Result<(), UsbError> + Send + Sync>,
    on_error: Arc<dyn Fn(UsbError) + Send + Sync>,
    state: Mutex<State>,
}

struct State {
    active: bool,
    pending: bool,
    running: bool,
    timer_pending: bool,
    generation: u64,
    cancel: Option<Box<dyn ScheduledTask>>,
}

impl Debouncer {
    pub(crate) fn new(
        scheduler: Arc<dyn TimerScheduler>,
        cooldown: Duration,
        immediate: bool,
        job: Arc<dyn Fn() -> Result<(), UsbError> + Send + Sync>,
        on_error: Arc<dyn Fn(UsbError) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            scheduler,
            cooldown,
            immediate,
            job,
            on_error,
            state: Mutex::new(State {
                active: true,
                pending: false,
                running: false,
                timer_pending: false,
                generation: 0,
                cancel: None,
            }),
        })
    }

    pub(crate) fn request(self: &Arc<Self>) -> Result<DebounceRequest, UsbError> {
        let mut cancel = None;
        let mut arm_generation = None;
        let run_now;
        {
            let mut state = lock(&self.state);
            if !state.active {
                return Ok(DebounceRequest::Ignored);
            }
            state.pending = true;
            if state.timer_pending {
                return Ok(DebounceRequest::Queued);
            }
            if self.immediate && !state.running {
                state.pending = false;
                state.running = true;
                run_now = true;
            } else {
                run_now = false;
                let (generation, old_cancel) = prepare_timer(&mut state);
                arm_generation = Some(generation);
                cancel = old_cancel;
            }
        }
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        if let Some(generation) = arm_generation {
            self.install_timer(generation);
        }
        if run_now {
            self.run_and_finish(false)?;
            Ok(DebounceRequest::Ran)
        } else {
            Ok(DebounceRequest::Queued)
        }
    }

    pub(crate) fn shutdown(&self) {
        let cancel = {
            let mut state = lock(&self.state);
            if !state.active {
                return;
            }
            state.active = false;
            state.pending = false;
            state.timer_pending = false;
            state.generation = state.generation.wrapping_add(1);
            state.cancel.take()
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    fn install_timer(self: &Arc<Self>, generation: u64) {
        let weak: Weak<Self> = Arc::downgrade(self);
        let handle = self.scheduler.schedule(
            self.cooldown,
            Box::new(move || {
                if let Some(debouncer) = weak.upgrade() {
                    debouncer.timer_fired(generation);
                }
            }),
        );
        let cancel_now = {
            let mut state = lock(&self.state);
            if state.active && state.timer_pending && state.generation == generation {
                state.cancel = Some(handle);
                None
            } else {
                Some(handle)
            }
        };
        if let Some(handle) = cancel_now {
            handle.cancel();
        }
    }

    fn timer_fired(self: &Arc<Self>, generation: u64) {
        let should_run = {
            let mut state = lock(&self.state);
            if !state.active || !state.timer_pending || state.generation != generation {
                return;
            }
            state.timer_pending = false;
            state.cancel = None;
            if !state.pending || state.running {
                false
            } else {
                state.pending = false;
                state.running = true;
                true
            }
        };
        if should_run {
            let _ = self.run_and_finish(true);
        }
    }

    fn run_and_finish(&self, report_error: bool) -> Result<(), UsbError> {
        let result = catch_unwind(AssertUnwindSafe(|| (self.job)()))
            .unwrap_or_else(|_| Err(UsbError::new("USB scan callback panicked")));
        let cancel = {
            let mut state = lock(&self.state);
            state.running = false;
            if state.active {
                let (_, old_cancel) = prepare_timer(&mut state);
                old_cancel
            } else {
                None
            }
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        // The generation was prepared above; install its timer after the lock
        // is released so a scheduler callback can never re-enter this mutex.
        let generation = {
            let state = lock(&self.state);
            (state.active && state.timer_pending).then_some(state.generation)
        };
        if let Some(generation) = generation {
            // `self` is owned by an Arc while run_and_finish is reached from a
            // scheduler. Immediate callers also retain the manager's Arc.
            self.install_timer_for_generation(generation);
        }
        if report_error && let Err(error) = &result {
            (self.on_error)(error.clone());
        }
        result
    }

    fn install_timer_for_generation(&self, generation: u64) {
        // The callback captures a weak reference. The scheduler task cannot
        // keep the integration alive past its owner.
        let weak = self.self_weak();
        let handle = self.scheduler.schedule(
            self.cooldown,
            Box::new(move || {
                if let Some(debouncer) = weak.upgrade() {
                    debouncer.timer_fired(generation);
                }
            }),
        );
        let cancel_now = {
            let mut state = lock(&self.state);
            if state.active && state.timer_pending && state.generation == generation {
                state.cancel = Some(handle);
                None
            } else {
                Some(handle)
            }
        };
        if let Some(handle) = cancel_now {
            handle.cancel();
        }
    }

    fn self_weak(&self) -> Weak<Self> {
        self.self_ref.clone()
    }
}

fn prepare_timer(state: &mut State) -> (u64, Option<Box<dyn ScheduledTask>>) {
    state.generation = state.generation.wrapping_add(1);
    state.timer_pending = true;
    (state.generation, state.cancel.take())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
