use crate::system_state::ErasedSystemStateCell;
use crate::EcsAccessError;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::sync::atomic::{AtomicBool, Ordering};
use bevy_platform::sync::{Arc, Mutex};
use core::marker::PhantomData;
use std::task::Waker;

pub(crate) struct Runner<Param: SystemParam + 'static, Func, Out> {
    cancelled: AtomicBool,
    system_state: Arc<dyn ErasedSystemStateCell>,
    inner: Mutex<RunnerInner<Func, Out>>,
    _p: PhantomData<fn() -> Param>,
}

struct RunnerInner<Func, Out> {
    run_fn: Option<Func>,
    waker: Option<Waker>,
    result: Option<Result<Out, EcsAccessError>>,
}

impl<Param: SystemParam + 'static, Func, Out> Runner<Param, Func, Out> {
    pub(crate) fn new(run_fn: Func, system_state: Arc<dyn ErasedSystemStateCell>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            system_state,
            inner: Mutex::new(RunnerInner {
                run_fn: Some(run_fn),
                waker: None,
                result: None,
            }),
            _p: PhantomData,
        }
    }

    pub(crate) fn take_result_or_set_waker(
        &self,
        waker: &Waker,
    ) -> Option<Result<Out, EcsAccessError>> {
        let mut state = self.inner.lock().unwrap();
        if state.result.is_none() {
            state.waker = Some(waker.clone());
        }
        state.result.take()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub(crate) enum RunResult {
    Completed,
    NeedsRetry,
    Cancelled,
}

pub(crate) trait ErasedRunner: Send + Sync {
    /// Attempts to run closure. Returns the outcome as a `RunResult`.
    /// Invariants:
    ///   If the closure was completed or canceled, caller can safely drop this trait object
    ///   If the closure needs to be retried, caller must eventually retry it
    fn try_run(&self, world: &mut World) -> RunResult;

    fn wake(&self);
    fn apply(&self, world: &mut World);
}

impl<Param, Func, Out> ErasedRunner for Runner<Param, Func, Out>
where
    Param: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Param::Item<'w, 's>) -> Out,
    Func: Send,
    Out: Send,
{
    fn try_run(&self, world: &mut World) -> RunResult {
        if self.is_cancelled() {
            return RunResult::Cancelled;
        }

        let mut runner = self.inner.lock().unwrap();
        if runner.result.is_some() {
            return RunResult::Completed;
        }

        let Some(mut system_state) = self.system_state.try_lock::<Param>(world) else {
            return RunResult::NeedsRetry;
        };

        // Take the func; we're committed to running or erroring.
        // Invariant: pending is always Some until we take it here (we hold the mutex).
        let func = runner.run_fn.take().unwrap();
        runner.result = Some(match system_state.get_mut(world) {
            Ok(param) => crate::invoke(func, param),
            Err(e) => Err(EcsAccessError::SystemParamValidation(e)),
        });
        RunResult::Completed
    }

    fn wake(&self) {
        let mut state = self.inner.lock().unwrap();
        if let Some(w) = state.waker.take() {
            // Release the mutex before waking. On single-threaded executors (e.g. wasm),
            // the woken task may poll inline and re-lock this mutex immediately.
            drop(state);
            w.wake();
        }
    }

    fn apply(&self, world: &mut World) {
        self.system_state.apply(world);
    }
}
