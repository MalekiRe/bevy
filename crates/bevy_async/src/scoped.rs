use crate::bridge::BridgeState;
use crate::guarded_latch::{LatchGuard, LatchWaiter};
use crate::system_state_cell::ErasedSystemStateCell;
use crate::{AsyncAccessError, AsyncParams};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::prelude::Vec;
use bevy_platform::sync::{Arc, Weak};
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

#[cfg(feature = "std")]
pub(crate) struct ScopedRequest {
    waker: Waker,
    latch: LatchWaiter,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl ScopedRequest {
    pub(crate) fn ensure_initialized(&self, world: &mut World) {
        self.system_state.ensure_initialized(world);
    }

    pub(crate) fn wake(self) -> WokenRequest {
        let Self {
            waker,
            latch,
            system_state,
        } = self;
        waker.wake();
        WokenRequest {
            latch,
            system_state,
        }
    }
}

pub(crate) struct WokenRequest {
    latch: LatchWaiter,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl WokenRequest {
    pub fn wait(self) -> Arc<dyn ErasedSystemStateCell> {
        let Self {
            latch,
            system_state,
        } = self;
        latch.wait();
        system_state
    }
}

/// Drains the scoped request queue for one sync point, scopes `&mut World`,
/// wakes futures so they can run their closures inline, then waits on each
/// latch before un-scoping. Returns the number of requests processed.
pub(crate) fn tick_scoped_queue(
    queue: &concurrent_queue::ConcurrentQueue<ScopedRequest>,
    scoped_world: &scoped_static_storage::ScopedStatic<World>,
    world: &mut World,
) -> usize {
    let mut scoped_batch = Vec::with_capacity(queue.len());
    while let Ok(req) = queue.pop() {
        scoped_batch.push(req);
    }
    if scoped_batch.is_empty() {
        return 0;
    }

    for req in &scoped_batch {
        req.ensure_initialized(world);
    }

    let count = scoped_batch.len();
    let system_states = scoped_world.scope(world, || {
        // Wake all futures first, then wait on all latches.
        // Separating wake from wait allows maximum parallelism on
        // multithreaded executors.
        let woken: Vec<_> = scoped_batch.into_iter().map(ScopedRequest::wake).collect();

        #[cfg(feature = "bevy_tasks")]
        bevy_tasks::tick_global_task_pools_on_main_thread();

        woken
            .into_iter()
            .map(WokenRequest::wait)
            .collect::<Vec<_>>()
    });

    for system_state in system_states {
        system_state.apply(world);
    }

    count
}

#[cfg(feature = "std")]
pub(crate) struct ScopedFut<Params: SystemParam + 'static, Func, Out> {
    sync_point_key: InternedSystemSet,
    world_fn: Option<Func>,
    maybe_poll_guard: Option<LatchGuard>,
    system_state: Arc<dyn ErasedSystemStateCell>,
    bridge: Weak<BridgeState>,
    _p: PhantomData<fn(Params) -> Out>,
}

impl<Func, Params: SystemParam + 'static, Out> ScopedFut<Params, Func, Out> {
    pub(crate) fn new(
        sync_point_key: InternedSystemSet,
        world_fn: Func,
        params: &AsyncParams<Params>,
    ) -> Self {
        Self {
            sync_point_key,
            world_fn: Some(world_fn),
            maybe_poll_guard: None,
            system_state: params.system_state.clone(),
            bridge: params.bridge.clone(),
            _p: PhantomData,
        }
    }
}

// None of the fields are self-referential.
#[cfg(feature = "std")]
impl<Params: SystemParam + 'static, Func, Out> Unpin for ScopedFut<Params, Func, Out> {}

#[cfg(feature = "std")]
impl<Params, Func, Out> Future for ScopedFut<Params, Func, Out>
where
    Params: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Params::Item<'w, 's>) -> Out,
{
    type Output = Result<Out, AsyncAccessError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Grab the poll guard (if any) so that we can drop it when
        // poll exits in any way, signaling the driver's latch from the
        // previous wake cycle.
        let _maybe_poll_guard = this.maybe_poll_guard.take();

        let bridge = match this.bridge.upgrade() {
            None => return Poll::Ready(Err(AsyncAccessError::WorldDropped)),
            Some(b) => b,
        };

        // Try to access the scoped world. If the driver is currently inside
        // `scope()`, we can run our closure directly with `&mut World`.
        //
        // Binding to a variable ensures the closure (and its borrows on
        // `this.system_state` / `this.world_fn`) is dropped before the
        // `None` arm, where we need `this` again.
        let result = {
            let system_state = &this.system_state;
            let world_fn = &mut this.world_fn;
            bridge
                .scoped_world
                .try_with(|world| {
                    system_state.ensure_initialized(world);
                    let Some(mut system_state) = system_state.try_lock::<Params>() else {
                        return None;
                    };
                    let params = match system_state.get_mut(world) {
                        Ok(params) => params,
                        Err(e) => return Some(Err(AsyncAccessError::InvalidParam(e))),
                    };
                    Some(Ok(world_fn.take().unwrap()(params)))
                })
                .ok()
                .flatten()
        };

        match result {
            Some(result) => Poll::Ready(result),
            None => {
                // World not scoped or system state contended.
                // Enqueue ourselves for the next driver tick.
                let (latch, guard) = LatchGuard::new_pair();
                this.maybe_poll_guard = Some(guard);

                let request = ScopedRequest {
                    waker: cx.waker().clone(),
                    latch,
                    system_state: this.system_state.clone(),
                };
                if bridge
                    .scoped_request_queues
                    .try_send(&this.sync_point_key, request)
                    .is_err()
                {
                    return Poll::Ready(Err(AsyncAccessError::WorldDropped));
                }
                Poll::Pending
            }
        }
    }
}
