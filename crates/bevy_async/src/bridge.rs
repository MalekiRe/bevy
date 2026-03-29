use crate::system_state::ErasedSystemStateCell;
use crate::wake_signal::WakeSignal;
use crate::world::{AsyncSystemState, AsyncWorld, TickResult};
use crate::EcsAccessError;
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::prelude::Vec;
use bevy_platform::sync::Arc;
use concurrent_queue::ConcurrentQueue;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use scoped_static_storage::ScopedStatic;

pub(crate) struct BridgeFut<Param: SystemParam + 'static, Func, Out> {
    _p: PhantomData<(Param, Out)>,
    sync_point_key: InternedSystemSet,
    bridge_fn: Option<Func>,
    wake_signal: Option<WakeSignal>,
    system_state: Arc<dyn ErasedSystemStateCell>,
    world: AsyncWorld,
}

impl<Params: SystemParam + 'static, Func, Out> BridgeFut<Params, Func, Out> {
    pub(crate) fn new(
        sync_point_key: InternedSystemSet,
        bridge_fn: Func,
        state: &AsyncSystemState<Params>,
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

impl<Param: SystemParam + 'static, Func, Out> Unpin for BridgeFut<Param, Func, Out> {}

impl<Param, Func, Out> Future for BridgeFut<Param, Func, Out>
where
    Param: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(Param::Item<'w, 's>) -> Out,
{
    type Output = Result<Out, EcsAccessError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _drop_at_end_of_scope = self.wake_signal.take();

        let strong_world = match self.world.0.upgrade() {
            None => return Poll::Ready(Err(EcsAccessError::WorldDropped)),
            Some(b) => b,
        };

        let result = strong_world
            .world_scope
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

                Some(Ok(bridge_fn.take().unwrap()(param)))
            })
            .ok()
            .flatten();

        match result {
            Some(result) => Poll::Ready(result),
            None => {
                let wake_signal = WakeSignal::new();
                self.wake_signal.replace(wake_signal.clone());
                let request = BridgeRequest {
                    waker: cx.waker().clone(),
                    wake_signal,
                    system_state: self.system_state.clone(),
                };
                match strong_world
                    .bridge_requests
                    .try_send(&self.sync_point_key, request)
                {
                    Ok(_) => Poll::Pending,
                    Err(_) => Poll::Ready(Err(EcsAccessError::WorldDropped)),
                }
            }
        }
    }
}

pub(crate) struct BridgeRequest {
    pub(crate) waker: Waker,
    pub(crate) wake_signal: WakeSignal,
    pub(crate) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl BridgeRequest {
    fn wake(self) -> WokenBridgeRequest {
        self.waker.wake();
        WokenBridgeRequest {
            wake_signal: self.wake_signal,
            system_state: self.system_state,
        }
    }
}

pub struct WokenBridgeRequest {
    wake_signal: WakeSignal,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl WokenBridgeRequest {
    fn wait(self) -> Arc<dyn ErasedSystemStateCell> {
        self.wake_signal.wait();
        self.system_state
    }
}

#[inline]
pub(crate) fn tick_bridge_queue(
    queue: &ConcurrentQueue<BridgeRequest>,
    scoped_world: &ScopedStatic<World>,
    world: &mut World,
) -> TickResult {
    let batch = queue.try_iter().collect::<Vec<_>>();
    if batch.is_empty() {
        return TickResult::NoWork;
    }

    let system_states = scoped_world.scope(world, || {
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

    TickResult::DidWork
}
