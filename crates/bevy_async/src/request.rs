use crate::system_state_cell::ErasedSystemStateCell;
use bevy_ecs::prelude::World;
use bevy_ecs::schedule::InternedSystemSet;
use bevy_platform::sync::Arc;

#[derive(Default)]
pub(crate) struct RequestQueues {
    inner: keyed_concurrent_queue::KeyedQueues<InternedSystemSet, PendingRequest>,
}

impl RequestQueues {
    // requires the world to initialize states
    pub(crate) fn drain_queue(
        &self,
        sync_point_key: InternedSystemSet,
        world: &mut World,
    ) -> PendingRequestBatch {
        let mut pending_request_batch = bevy_platform::prelude::vec![];
        while let Ok(pending_request) = self.inner.get_or_create(&sync_point_key).pop() {
            pending_request.system_state.ensure_initialized(world);
            pending_request_batch.push(pending_request);
        }
        PendingRequestBatch(pending_request_batch)
    }

    pub(crate) fn try_send(
        &self,
        sync_point_key: InternedSystemSet,
        request: PendingRequest,
    ) -> Result<(), PendingRequest> {
        self.inner
            .try_send(&sync_point_key, request)
            .map_err(|p| p.into_inner())
    }
}

// invariant: if non-empty, must not be dropped (call wake_all() instead)
pub(crate) struct PendingRequestBatch(bevy_platform::prelude::Vec<PendingRequest>);

impl PendingRequestBatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    // invariant: you can only call this when the world is published
    pub(crate) fn wake_all(self) -> WokenRequests {
        WokenRequests(
            self.0
                .into_iter()
                .map(PendingRequest::wake)
                // we re-collect to ensure we fully exhaust the prior iterator
                // we want to have all the wakers call .wake() before waiting on the first latch
                .collect(),
        )
    }
}

// invariant: must not be dropped (call wait_all() instead)
pub(crate) struct WokenRequests(bevy_platform::prelude::Vec<WokenRequest>);

impl WokenRequests {
    // invariant: you can only call this when the world is published
    pub(crate) fn wait_all(self) -> PolledRequests {
        PolledRequests(
            self.0
                .into_iter()
                .map(WokenRequest::wait)
                // we re-collect to ensure all latches are waited before returning.
                .collect(),
        )
    }
}

pub(crate) struct PolledRequests(bevy_platform::prelude::Vec<PolledRequest>);

// invariant: must not be dropped (call apply() instead)
impl PolledRequests {
    pub(crate) fn apply(self, world: &mut World) {
        for request in self.0 {
            request.apply(world);
        }
    }
}

/// A pending access request bridging an async task into ECS.
pub(crate) struct PendingRequest {
    /// Waker for the async future that wants ECS access.
    /// When the `SyncPoint` is driven, this waker is fired so the future can
    /// poll while `scoped_world` exposes the current `World`.
    pub(crate) waker: core::task::Waker,
    /// Our custom primitive that lets us wait until all the futures have tried to run before
    /// continuing.
    pub(crate) latch: crate::guarded_latch::LatchWaiter,
    pub(crate) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl PendingRequest {
    fn wake(self) -> WokenRequest {
        // Trigger the async future so it can poll while `scoped_world`
        // is active.
        self.waker.wake();
        WokenRequest {
            system_state: self.system_state,
            latch: self.latch,
        }
    }
}

/// A request whose waker has already been fired.
struct WokenRequest {
    latch: crate::guarded_latch::LatchWaiter,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl WokenRequest {
    fn wait(self) -> PolledRequest {
        self.latch.wait();
        PolledRequest {
            system_state: self.system_state,
        }
    }
}

/// A request that has finished its attempted poll and may need to apply deferred world state.
struct PolledRequest {
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl PolledRequest {
    #[inline]
    fn apply(self, world: &mut World) {
        self.system_state.apply(world);
    }
}
