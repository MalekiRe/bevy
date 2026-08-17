use crate::bridge_future::BridgeFunctionState;
use crate::plugin::StrongAsyncWorld;
use bevy_ecs::prelude::{IntoSystemSet, SystemSet, World};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_platform::sync::{Arc, Mutex, PoisonError};

/// An exclusive system that drives the queued bridge work for `SyncPoint`.
///
/// Every queued bridge request will be run. The return value will be sent back to the associated
/// bridge future, and the future will be woken.
///
/// The `SyncPoint` is an arbitrary, user-defined type that acts as a "label" for a sync point.
/// Async tasks must use the same type in [`AsyncSystemState::bridge`] to use this sync point. This
/// allows bridging to different points in the ECS schedule so the actions apply at correct points.
///
/// [`AsyncSystemState::bridge`]: crate::AsyncSystemState::bridge
pub fn async_world_sync_point<SyncPoint: 'static>(world: &mut World) {
    // Derive the stable interned system-set key used to look up requests queued
    // for this exact sync point type.
    let sync_point = async_world_sync_point::<SyncPoint>
        .into_system_set()
        .intern();
    let async_world = world.get_resource::<StrongAsyncWorld>().unwrap().clone();

    // This ticks a single sync point, fetching all the requests for the sync point and then running
    // each request one at a time.
    let this = &async_world.0;
    let mut queued_requests = bevy_platform::prelude::vec![];
    while let Ok(queued_task_bridge) = this.bridge_requests.get_or_create(&sync_point).pop() {
        queued_requests.push(queued_task_bridge);
    }
    if queued_requests.is_empty() {
        return;
    }
    execute_requests(world, queued_requests);
}

#[derive(Default)]
pub(crate) struct AsyncWorldInner {
    pub(crate) bridge_requests:
        keyed_concurrent_queue::KeyedQueues<InternedSystemSet, BridgeRequest>,
}

/// We need to notify all our Wakers that have queued that we've dropped so they can error
impl Drop for AsyncWorldInner {
    fn drop(&mut self) {
        for bridge_requests in self.bridge_requests.inner().read().unwrap().values() {
            while let Ok(request) = bridge_requests.pop() {
                request.waker.wake();
            }
        }
    }
}

/// A queued access request bridging an async task into ECS.
pub(crate) struct BridgeRequest {
    /// The bridge function that should be executed once we have ECS access.
    pub(crate) bridge_fn: Arc<Mutex<BridgeFunctionState>>,
    /// Waker for the [`crate::bridge_future::BridgeFuture`] that wants ECS access.
    ///
    /// This allows the future to be woken up once its `bridge_fn` has run.
    pub(crate) waker: core::task::Waker,
}

#[inline]
fn execute_requests(
    world: &mut World,
    queued_requests: bevy_platform::prelude::Vec<BridgeRequest>,
) {
    // Because currently we do not run non-conflicting system param requests in parallel (this is
    // follow up work) we can run each request in order.
    for BridgeRequest { bridge_fn, waker } in queued_requests {
        let mut bridge_fn = bridge_fn.lock().unwrap_or_else(PoisonError::into_inner);
        #[expect(
            unsafe_code,
            reason = "we only know this is safe because of how bridge_fn happens to be called here"
        )]
        // SAFETY: This is our first (and only) time running this request, so it is either in state
        // `Runnable`, or it is in state `Terminated`. If it's in the latter, nothing happens, so we
        // are safe. If it is `Runnable`, that means that the associated `BridgeFuture` hasn't been
        // cancelled (since on Drop, it sets the state to `Terminated`). Therefore, the lifetimes in
        // its `Func` generic argument will bound it, meaning the references in the function are
        // valid. Therefore, it is safe to call this function.
        unsafe {
            bridge_fn.run(world);
        }
        drop(bridge_fn);
        // Wake the task after we're done locking the bridge so we don't accidentally block the
        // task, adding to latency.
        waker.wake();
    }
}
