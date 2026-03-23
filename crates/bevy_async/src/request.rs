use crate::system_state_store::ErasedStateStore;
use bevy_ecs::prelude::World;
use bevy_ecs::schedule::InternedSystemSet;
use bevy_platform::sync::Arc;

pub(crate) type RequestQueues =
    keyed_concurrent_queue::KeyedQueues<InternedSystemSet, PendingRequest>;

/// A pending access request bridging an async task into ECS.
pub(crate) struct PendingRequest {
    /// Waker for the async future that wants ECS access.
    /// When the `SyncPoint` is driven, this waker is fired so the future can
    /// poll while `scoped_world` exposes the current `World`.
    pub(crate) waker: core::task::Waker,
    /// Our custom primitive that lets us wait until all the futures have tried to run before
    /// continuing.
    pub(crate) latch: crate::guarded_latch::LatchWaiter,
    pub(crate) already_ready: bool,
    pub(crate) system_state: Arc<dyn ErasedStateStore>,
}

impl PendingRequest {
    /// Initialize the `TypedStateStore` if it isn't already initialized.
    pub(crate) fn ensure_system_state_initialized(mut self, world: &mut World) -> Self {
        if self.already_ready {
            return self;
        }
        self.system_state.initialize(world);
        self.already_ready = true;
        self
    }
}

/// A request whose waker has already been fired.
struct WokenRequest {
    latch: crate::guarded_latch::LatchWaiter,
    system_state: Arc<dyn ErasedStateStore>,
}

/// A request that has finished its attempted poll and may need to apply deferred world state.
pub(crate) struct PolledRequest {
    system_state: Arc<dyn ErasedStateStore>,
}

impl PolledRequest {
    #[inline]
    pub fn apply(self, world: &mut World) {
        self.system_state.apply(world);
    }
}

#[inline]
pub fn wake_all_and_collect(
    pending_requests: bevy_platform::prelude::Vec<PendingRequest>,
) -> bevy_platform::prelude::Vec<PolledRequest> {
    let woken_requests = pending_requests
        .into_iter()
        .map(
            |PendingRequest {
                 system_state,
                 waker,
                 latch,
                 ..
             }| {
                // Trigger the async future so it can poll while `scoped_world`
                // is active.
                waker.wake();
                WokenRequest {
                    system_state,
                    latch,
                }
            },
        )
        // we re-collect to ensure we fully exhaust the prior iterator
        // we want to have all the wakers call .wake() before waiting on the first latch
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
                 latch,
             }| {
                latch.wait();
                PolledRequest { system_state }
            },
        )
        .collect()
}
