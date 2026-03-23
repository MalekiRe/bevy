use crate::plugin::{AsyncBridge, AsyncTickBudget};
use crate::system_state_store::ErasedStateStore;
use bevy_ecs::prelude::{IntoSystemSet, SystemSet, World};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_platform::sync::Arc;

/// Drives the queued bridge work for `SyncPoint`.
///
/// Every queued access request is guaranteed to be *woken*. That wake guarantees the corresponding
/// async future gets a chance to poll.
/// It does *not* however guarantee the poll will finish its ECS work, because that
/// poll may still fail to finish it's work for a *variety* of reasons, i.e. it is unable to acquire
/// the typed `SystemState` lock and returns `Poll::Pending`.
///
/// This function attempts to drive queued work several times, up to
/// `AsyncTickBudget`. If one internal tick finds no work, we opportunistically tick the
/// global task pool and try once more before returning early.
///
/// We drive queued work multiple times for two reasons. The first is that serial `.await` calls
/// should try to all be completed within the same `SyncPoint` such as
/// ```rust,ignore
/// let health = task_1.run(|health: Single<&Health, With<Player>>| {
///     health.0
/// }).await;
/// if health == 0 {
///     return;
/// }
/// task_1.run(|commands: Commands| {
///     commands.trigger(PlayerDoesAttack);
/// }).await;
/// ```
/// The second reason is spoken of prior. Poll may fail to finish for a variety of reasons and
/// should be given several chances before quitting.
pub fn tick_async_bridge<SyncPoint: 'static>(world: &mut World) {
    // Derive the stable interned system-set key used to look up requests queued
    // for this exact sync point type.
    let sync_point_key = tick_async_bridge::<SyncPoint>.into_system_set().intern();
    let bridge = world.get_resource::<AsyncBridge>().unwrap().clone();
    // Read the configured maximum number of internal attempts we are willing to
    // perform during this `SyncPoint`.
    let max_ticks = world.get_resource::<AsyncTickBudget>().unwrap().0;
    for _ in 0..max_ticks {
        // Drive once. If no work was found, we may truly be done.
        // but we should give external task pools one more opportunity to make newly-woken
        // tasks runnable.
        if bridge.0.tick_sync_point(sync_point_key, world) == TickResult::NoWork {
            #[cfg(feature = "bevy_tasks")]
            bevy_tasks::cfg::web! {
                if {} else {
                    bevy_tasks::tick_global_task_pools_on_main_thread();
                }
            }
            // Retry once after ticking the global pool. If we are still idle,
            // there is no more immediately available progress to make.
            if bridge.0.tick_sync_point(sync_point_key, world) == TickResult::NoWork {
                return;
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct BridgeState {
    pub(crate) request_queues:
        keyed_concurrent_queue::KeyedQueues<InternedSystemSet, PendingRequest>,
    pub(crate) scoped_world: scoped_static_storage::ScopedStatic<World>,
}

impl BridgeState {
    /// This drives a single sync point, requesting the poll of all tasks in that sync point.
    /// None of the tasks are guaranteed to actually return `Poll::Ready`, but all are guaranteed to
    /// at least do a `Poll::Pending`
    ///
    /// The flow of logic is the following:
    /// 1. We first drain the queue for our `SyncPoint`.
    /// 2. We initialize the request's `SystemState`. (This is idempotent).
    /// 3. Expose our `World` through `scoped_world`.
    /// 4. Wake all our `AsyncSystemHandleFut`s.
    /// 5. Apply our `SystemState` back into the `World`. (Things like `Commands`).
    fn tick_sync_point(&self, sync_point_key: InternedSystemSet, world: &mut World) -> TickResult {
        let mut pending_request_batch = bevy_platform::prelude::vec![];
        while let Ok(mut pending_request) =
            self.request_queues.get_or_create(&sync_point_key).pop()
        {
            pending_request_batch.push(pending_request.ensure_system_state_initialized(world));
        }
        // If no requests were waiting then report idle so the caller can decide whether to stop
        // or attempt one more task-pool tick.
        if pending_request_batch.is_empty() {
            return TickResult::NoWork;
        }
        // Make this `World` temporarily visible to our waking futures. Wake them all and wait
        // until they all have at least *attempted* to poll.
        // This is contractually obligated by the contract of `.wake()`. We are guaranteed one wake
        // per call to our `.wake()`.
        let polled_requests = self
            .scoped_world
            .scope(world, || wake_all_and_collect(pending_request_batch));
        for request in polled_requests {
            request.apply(world);
        }
        TickResult::DidWork
    }
}

/// Whether a tick attempt did any work.
#[derive(PartialEq)]
enum TickResult {
    /// We found and processed at least one queued request.
    DidWork,
    /// There was no queued work available for the `SyncPoint`.
    NoWork,
}

/// A pending access request bridging an async task into ECS.
pub(crate) struct PendingRequest {
    /// Waker for the async future that wants ECS access.
    /// When the `SyncPoint` is driven, this waker is fired so the future can
    /// poll while `scoped_world` exposes the current `World`.
    pub(crate) waker: core::task::Waker,
    /// Our custom primitive that lets us wait until all the futures have tried to run before
    /// continuing.
    pub(crate) poll_signal: crate::poll_signal::PollSignal,
    pub(crate) already_ready: bool,
    pub(crate) system_state: Arc<dyn ErasedStateStore>,
}

/// A request whose waker has already been fired.
struct WokenRequest {
    poll_signal: crate::poll_signal::PollSignal,
    system_state: Arc<dyn ErasedStateStore>,
}

/// A request that has finished its attempted poll and may need to apply deferred world state.
struct PolledRequest {
    system_state: Arc<dyn ErasedStateStore>,
}

impl PolledRequest {
    #[inline]
    fn apply(self, world: &mut World) {
        self.system_state.apply(world);
    }
}

impl PendingRequest {
    /// Initialize the `TypedStateStore` if it isn't already initialized.
    fn ensure_system_state_initialized(mut self, world: &mut World) -> Self {
        if self.already_ready {
            return self;
        }
        self.system_state.initialize(world);
        self.already_ready = true;
        self
    }
}

#[inline]
fn wake_all_and_collect(
    pending_requests: bevy_platform::prelude::Vec<PendingRequest>,
) -> bevy_platform::prelude::Vec<PolledRequest> {
    let woken_requests = pending_requests
        .into_iter()
        .map(
            |PendingRequest {
                 system_state,
                 waker,
                 poll_signal,
                 ..
             }| {
                // Trigger the async future so it can poll while `scoped_world`
                // is active.
                waker.wake();
                WokenRequest {
                    system_state,
                    poll_signal,
                }
            },
        )
        // we re-collect to ensure we fully exhaust the prior iterator
        // we want to have all the wakers call .wake() before waiting on the first signal
        .collect::<bevy_platform::prelude::Vec<_>>();

    #[cfg(feature = "bevy_tasks")]
    bevy_tasks::cfg::web! {
        if {} else {
            bevy_tasks::tick_global_task_pools_on_main_thread();
        }
    }

    woken_requests
        .into_iter()
        .map(
            |WokenRequest {
                 system_state,
                 poll_signal,
             }| {
                poll_signal.wait();
                PolledRequest { system_state }
            },
        )
        .collect()
}
