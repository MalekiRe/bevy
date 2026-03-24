use crate::bridge::BridgeState;
use crate::system_state_cell::ErasedSystemStateCell;
use crate::{AsyncAccessError, AsyncParams};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::prelude::Vec;
use bevy_platform::sync::atomic::{AtomicBool, Ordering};
use bevy_platform::sync::{Arc, ConditionalSend, Mutex, Weak};
use concurrent_queue::ConcurrentQueue;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

pub(crate) enum JobResult {
    Completed,
    NeedsRetry,
    Cancelled,
}

pub(crate) trait ErasedJob: ConditionalSend + Sync {
    /// Attempts to run the job's closure. Returns the outcome as a `JobResult`.
    /// Invariants:
    ///   If the job was completed or canceled, caller can safely drop the job
    ///   If the job needs to be retried, caller must eventually retry the job
    fn try_run(&self, world: &mut World) -> JobResult;

    fn wake(&self);
    fn apply(&self, world: &mut World);
}

pub(crate) struct JobCell<Func, Params: SystemParam + 'static, Out> {
    cancelled: AtomicBool,
    system_state: Arc<dyn ErasedSystemStateCell>,
    inner: Mutex<Job<Func, Out>>,
    _p: PhantomData<fn() -> Params>,
}

struct Job<Func, Out> {
    pending: Option<Func>,
    waker: Option<Waker>,
    result: Option<Result<Out, AsyncAccessError>>,
}

impl<Func, Params: SystemParam + 'static, Out> JobCell<Func, Params, Out> {
    fn new(func: Func, system_state: Arc<dyn ErasedSystemStateCell>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            system_state,
            inner: Mutex::new(Job {
                pending: Some(func),
                waker: None,
                result: None,
            }),
            _p: PhantomData,
        }
    }

    fn take_result_or_set_waker(&self, waker: &Waker) -> Option<Result<Out, AsyncAccessError>> {
        let mut state = self.inner.lock().unwrap();
        if state.result.is_none() {
            state.waker = Some(waker.clone());
        }
        state.result.take()
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl<Func, Params, Out> ErasedJob for JobCell<Func, Params, Out>
where
    Params: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Params::Item<'w, 's>) -> Out,
    Func: ConditionalSend,
    Out: ConditionalSend,
{
    fn try_run(&self, world: &mut World) -> JobResult {
        if self.is_cancelled() {
            return JobResult::Cancelled;
        }

        self.system_state.ensure_initialized(world);

        let mut job = self.inner.lock().unwrap();
        if job.result.is_some() {
            return JobResult::Completed;
        }

        let Some(mut system_state) = self.system_state.try_lock::<Params>() else {
            return JobResult::NeedsRetry;
        };

        // Take the func; we're committed to running or erroring.
        // Invariant: pending is always Some until we take it here (we hold the mutex).
        let func = job.pending.take().unwrap();
        job.result = Some(match system_state.get_mut(world) {
            Ok(params) => invoke(func, params),
            Err(e) => Err(AsyncAccessError::InvalidParam(e)),
        });
        JobResult::Completed
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

/// Future representing a single in-flight ECS job.
///
/// The driver (sync-point system) runs the Func directly with `&mut World`.
/// This future just waits for the result to appear in the shared slot.
pub(crate) struct JobFut<Params: SystemParam + 'static, Func, Out> {
    sync_point_key: InternedSystemSet,
    job: Arc<JobCell<Func, Params, Out>>,
    bridge: Weak<BridgeState>,
    /// Whether we have already enqueued ourselves in the job queue.
    queued: bool,
}

impl<Func, Params: SystemParam + 'static, Out> JobFut<Params, Func, Out> {
    pub(crate) fn new(
        sync_point_key: InternedSystemSet,
        world_fn: Func,
        params: &AsyncParams<Params>,
    ) -> Self {
        Self {
            sync_point_key,
            job: Arc::new(JobCell::<Func, Params, Out>::new(
                world_fn,
                params.system_state.clone(),
            )),
            bridge: params.bridge.clone(),
            queued: false,
        }
    }
}

// none of the fields are self-referential
impl<Params: SystemParam + 'static, Func, Out> Unpin for JobFut<Params, Func, Out> {}

impl<Params, Func, Out> Future for JobFut<Params, Func, Out>
where
    Params: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Params::Item<'w, 's>) -> Out + 'static,
    Func: ConditionalSend,
    Out: ConditionalSend + 'static,
{
    type Output = Result<Out, AsyncAccessError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if the result is already available.
        if let Some(result) = self.job.take_result_or_set_waker(cx.waker()) {
            return Poll::Ready(result);
        }

        let this = self.get_mut();

        // Try to upgrade the bridge. If the world is gone, fail.
        let bridge = match this.bridge.upgrade() {
            None => return Poll::Ready(Err(AsyncAccessError::WorldDropped)),
            Some(bridge) => bridge,
        };

        // Enqueue if we haven't already.
        if !this.queued {
            this.queued = true;
            let job = this.job.clone() as Arc<dyn ErasedJob>;
            if bridge
                .job_queues
                .try_send(&this.sync_point_key, job)
                .is_err()
            {
                return Poll::Ready(Err(AsyncAccessError::WorldDropped));
            }
        }

        Poll::Pending
    }
}

impl<Params, Func, Out> Drop for JobFut<Params, Func, Out>
where
    Params: SystemParam + 'static,
{
    fn drop(&mut self) {
        // Signal cancellation so the driver skips this job.
        self.job.cancelled.store(true, Ordering::Release);
    }
}

/// Runs the user closure, catching panics so one bad closure doesn't take down
/// the entire driver batch (and the rest of the schedule too).
#[cfg(all(feature = "std", panic = "unwind"))]
#[inline(always)]
fn invoke<Func, Args, Out>(func: Func, args: Args) -> Result<Out, AsyncAccessError>
where
    Func: FnOnce(Args) -> Out,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func(args))) {
        Ok(out) => Ok(out),
        Err(_) => Err(AsyncAccessError::Panicked),
    }
}

/// Fallback when `catch_unwind` is unavailable. The panic propagates normally.
#[cfg(not(all(feature = "std", panic = "unwind")))]
#[inline(always)]
fn invoke<Func, Args, Out>(func: Func, args: Args) -> Result<Out, AsyncAccessError>
where
    Func: FnOnce(Args) -> Out,
{
    Ok(func(args))
}

/// Drains the job queue for one sync point and runs each closure with `&mut World`.
/// Returns the number of jobs completed.
pub(crate) fn tick_job_queue(
    queue: &ConcurrentQueue<Arc<dyn ErasedJob>>,
    world: &mut World,
) -> usize {
    let mut job_batch = Vec::with_capacity(queue.len());
    while let Ok(job) = queue.pop() {
        job_batch.push(job);
    }

    let mut completed = Vec::new();
    let mut needs_requeue = Vec::new();

    for job in job_batch {
        match job.try_run(world) {
            JobResult::Cancelled => continue,
            JobResult::NeedsRetry => needs_requeue.push(job),
            JobResult::Completed => completed.push(job),
        }
    }

    for job in needs_requeue {
        queue
            .push(job)
            .ok()
            .expect("queue should be sane, was the world dropped?");
    }

    let count = completed.len();
    for job in completed {
        job.apply(world);
        job.wake();
    }

    count
}
