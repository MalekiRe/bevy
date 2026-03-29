mod runner;

use crate::world::AsyncWorld;
use crate::{AsyncSystemState, EcsAccessError};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::prelude::Vec;
use bevy_platform::sync::Arc;
use core::pin::Pin;
use core::task::{Context, Poll};
use keyed_concurrent_queue::KeyedQueues;
use runner::{ErasedRunner, RunResult, Runner};

/// Future representing a single in-flight ECS RunFn.
///
/// The world-owning thread runs the Func directly with `&mut World`.
/// This future just waits for the result to appear in the shared slot.
pub(crate) struct RunnerFut<Param: SystemParam + 'static, Func, Out> {
    sync_point_key: InternedSystemSet,
    /// Inner runner state shared slot.
    runner: Arc<Runner<Param, Func, Out>>,
    world: AsyncWorld,
    /// Whether we have already enqueued ourselves in the run queue.
    queued: bool,
}

// none of the fields are self-referential
impl<Param: SystemParam + 'static, Func, Out> Unpin for RunnerFut<Param, Func, Out> {}

impl<Param, Func, Out> Future for RunnerFut<Param, Func, Out>
where
    Param: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Param::Item<'w, 's>) -> Out + 'static,
    Func: Send,
    Out: Send + 'static,
{
    type Output = Result<Out, EcsAccessError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if the result is already available.
        if let Some(result) = self.runner.take_result_or_set_waker(cx.waker()) {
            return Poll::Ready(result);
        }

        let strong_world_handle = match self.world.0.upgrade() {
            None => return Poll::Ready(Err(EcsAccessError::WorldDropped)),
            Some(w) => w,
        };

        // Enqueue if we haven't already.
        if !self.queued {
            self.queued = true;
            let runner = self.runner.clone() as Arc<dyn ErasedRunner>;
            let queue = strong_world_handle
                .run_state
                .runners
                .get_or_create(&self.sync_point_key);
            return match queue.push(runner) {
                Ok(_) => Poll::Pending,
                Err(_) => Poll::Ready(Err(EcsAccessError::WorldDropped)),
            };
        }

        Poll::Pending
    }
}

impl<Param, Func, Out> Drop for RunnerFut<Param, Func, Out>
where
    Param: SystemParam + 'static,
{
    fn drop(&mut self) {
        // Signal cancellation so the world thread skips this run.
        self.runner.cancel();
    }
}

impl<Param: SystemParam + 'static, Func, Out> RunnerFut<Param, Func, Out> {
    pub(crate) fn new(
        sync_point_key: InternedSystemSet,
        runner_fn: Func,
        state: &AsyncSystemState<Param>,
    ) -> Self {
        Self {
            sync_point_key,
            runner: Arc::new(Runner::<Param, Func, Out>::new(
                runner_fn,
                state.inner.clone(),
            )),
            world: state.world.clone(),
            queued: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct RunState {
    runners: KeyedQueues<InternedSystemSet, Arc<dyn ErasedRunner>>,
}

impl RunState {
    pub(crate) fn tick(&self, sync_point_key: InternedSystemSet, world: &mut World) -> usize {
        let queue = self.runners.get_or_create(&sync_point_key);
        let batch = queue.try_iter().collect::<Vec<_>>();
        if batch.is_empty() {
            return 0;
        }

        let mut completed = Vec::new();
        let mut needs_requeue = Vec::new();

        for runner in batch {
            match runner.try_run(world) {
                RunResult::Cancelled => continue,
                RunResult::NeedsRetry => needs_requeue.push(runner),
                RunResult::Completed => completed.push(runner),
            }
        }

        for runner in needs_requeue {
            queue
                .push(runner)
                .ok()
                .expect("queue should be sane, was the world dropped?");
        }

        let count = completed.len();
        for runner in completed {
            runner.apply(world);
            runner.wake();
        }

        count
    }
}
