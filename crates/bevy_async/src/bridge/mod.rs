use crate::system_state::ErasedSystemStateCell;
use crate::world::{AsyncSystemState, AsyncWorld};
use crate::EcsAccessError;
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::prelude::Vec;
use bevy_platform::sync::Arc;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};
use keyed_concurrent_queue::KeyedQueues;
use request::{BridgeRequest, WokenBridgeRequest};
use scoped_static_storage::ScopedStatic;
use wake_signal::WakeSignal;

mod request;
mod wake_signal;

pub(crate) struct BridgeFut<Param: SystemParam + 'static, Func, Out> {
    _p: PhantomData<(Param, Out)>,
    sync_point_key: InternedSystemSet,
    bridge_fn: Option<Func>,
    wake_signal: Option<WakeSignal>,
    system_state: Arc<dyn ErasedSystemStateCell>,
    world: AsyncWorld,
}

impl<Param: SystemParam + 'static, Func, Out> BridgeFut<Param, Func, Out> {
    pub(crate) fn new(
        sync_point_key: InternedSystemSet,
        bridge_fn: Func,
        state: &AsyncSystemState<Param>,
    ) -> Self {
        Self {
            sync_point_key,
            bridge_fn: Some(bridge_fn),
            wake_signal: None,
            system_state: state.inner.clone(),
            world: state.world.clone(),
            _p: PhantomData,
        }
    }
}

// None of the fields are self-referential.
impl<Param: SystemParam + 'static, Func, Out> Unpin for BridgeFut<Param, Func, Out> {}

impl<Param, Func, Out> Future for BridgeFut<Param, Func, Out>
where
    Param: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Param::Item<'w, 's>) -> Out,
{
    type Output = Result<Out, EcsAccessError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Grab the poll guard (if any) so that we can drop it when
        // poll exits in any way and send a signal to the waiter.
        let _drop_at_end_of_scope = self.wake_signal.take();

        let strong_world_handle = match self.world.0.upgrade() {
            None => return Poll::Ready(Err(EcsAccessError::WorldDropped)),
            Some(w) => w,
        };

        // Try to access the scoped world. If the world-owning thread is currently inside
        // `ScopedStatic::scope()`, we can run our closure directly with `&mut World`.
        match strong_world_handle
            .bridge_state
            .scoped_world
            .try_with(|world| {
                let Self {
                    ref system_state,
                    ref mut bridge_fn,
                    ..
                } = *self;

                let mut system_state = system_state.try_lock::<Param>(world)?;

                let param = match system_state.get_mut(world) {
                    Ok(param) => param,
                    Err(e) => return Some(Err(EcsAccessError::SystemParamValidation(e))),
                };

                // Invariant: This future shouldn't be polled after it returns Poll::Ready
                let func = bridge_fn.take().unwrap();
                Some(crate::invoke(func, param))
            })
            .ok()
            .flatten()
        {
            // Success! Finish the future with the result.
            Some(result) => Poll::Ready(result),
            None => {
                // World contended or not scoped, or system state contended.
                // Enqueue ourselves for the next tick.
                let wake_signal = WakeSignal::new();
                self.wake_signal.replace(wake_signal.clone());
                let request = BridgeRequest {
                    waker: cx.waker().clone(),
                    wake_signal,
                    system_state: self.system_state.clone(),
                };
                match strong_world_handle
                    .bridge_state
                    .requests
                    .try_send(&self.sync_point_key, request)
                {
                    Ok(_) => Poll::Pending,
                    Err(_) => Poll::Ready(Err(EcsAccessError::WorldDropped)),
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct BridgeState {
    requests: KeyedQueues<InternedSystemSet, BridgeRequest>,
    scoped_world: ScopedStatic<World>,
}

impl BridgeState {
    pub(crate) fn tick(&self, sync_point_key: InternedSystemSet, world: &mut World) -> usize {
        let queue = self.requests.get_or_create(&sync_point_key);
        let batch = queue.try_iter().collect::<Vec<_>>();
        if batch.is_empty() {
            return 0;
        }

        let count = batch.len();
        let system_states = self.scoped_world.scope(world, || {
            // Wake all futures first, then wait on all woken futures.
            // Separating wake from wait allows maximum parallelism on
            // multithreaded executors.
            let woken: Vec<_> = batch.into_iter().map(BridgeRequest::wake).collect();

            #[cfg(feature = "bevy_tasks")]
            bevy_tasks::tick_global_task_pools_on_main_thread();

            woken
                .into_iter()
                .map(WokenBridgeRequest::wait)
                .collect::<Vec<_>>()
        });

        for system_state in system_states {
            system_state.apply(world);
        }

        count
    }
}
