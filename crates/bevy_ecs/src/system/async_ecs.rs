use bevy_ecs::{
    prelude::{IntoSystemSet, NonSend, SystemSet},
    schedule::InternedSystemSet,
    system::{SystemParam, SystemParamValidationError, SystemState},
    world::World,
};
use bevy_platform::{
    prelude::Vec,
    sync::{Arc, Mutex, MutexGuard, Weak},
};
use core::{
    any::Any,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use derive_more::Deref;
use keyed_concurrent_queue::KeyedQueues;
use scoped_static_storage::ScopedStatic;
use std::sync::Condvar;
use thiserror::Error; // This is what prevents us from being no_std currently

#[derive(Clone)]
struct WakeSignal(Arc<(Mutex<bool>, Condvar)>);
impl WakeSignal {
    #[inline]
    pub fn new() -> Self {
        WakeSignal(Arc::new((Mutex::new(false), Condvar::new())))
    }
    #[inline]
    pub fn wait(&self) {
        let (lock, cv) = &*self.0;
        let mut signaled = lock.lock().unwrap();
        while !*signaled {
            signaled = cv.wait(signaled).unwrap();
        }
    }
}
impl Drop for WakeSignal {
    #[inline]
    fn drop(&mut self) {
        let (lock, cv) = &*self.0;
        let mut signaled = lock.lock().unwrap();
        *signaled = true;
        cv.notify_one();
    }
}

/// Add this system to a schedule and use it as you would normally, then do
/// `app.add_systems(Update, async_sync_point::<Marker>.after(other_system));`
/// `world_id.ecs_task().run_system(Marker, || {}).await;`
pub fn async_sync_point<Marker: 'static>(world: &mut World) {
    let interned = async_sync_point::<Marker>.into_system_set().intern();
    let async_ecs = world.get_resource::<AsyncEcs>().unwrap().clone();
    let emergency_exit_amount = world.get_resource::<EmergencyExitAmount>().unwrap().clone();
    for _ in 0..emergency_exit_amount.0 {
        if async_ecs.0.tick_async_tasks(interned, world) == TickAsyncTasksResult::NoMoreTasksToTick
        {
            bevy_tasks::cfg::web! {
                if {} else {
                    bevy_tasks::tick_global_task_pools_on_main_thread();
                }
            }
            if async_ecs.0.tick_async_tasks(interned, world)
                == TickAsyncTasksResult::NoMoreTasksToTick
            {
                return;
            }
        }
    }
}

#[derive(bevy_ecs_macros::Resource, Clone)]
pub struct AsyncEcs(Arc<AsyncEcsInternal>);
impl Default for AsyncEcs {
    fn default() -> Self {
        Self(Arc::new(AsyncEcsInternal {
            async_system_states: KeyedQueues::new(),
            world_access: ScopedStatic::new(),
        }))
    }
}

#[derive(bevy_ecs_macros::Resource, Clone, Deref)]
pub struct EmergencyExitAmount(pub usize);

struct AsyncEcsInternal {
    async_system_states: KeyedQueues<InternedSystemSet, AsyncSystemState>,
    world_access: ScopedStatic<World>,
}

#[derive(PartialEq)]
enum TickAsyncTasksResult {
    MoreTasksToTick,
    NoMoreTasksToTick,
}

impl AsyncEcsInternal {
    /// Returns `true` if there are no
    /// /// This function finds all pending `async_access` calls for a particular `Schedule` and a particular
    //     /// `WorldId`. It wakes all of them, temporarily and soundly stores a `UnsafeWorldCell` in the
    //     /// `GLOBAL_WORLD_ACCESS` and parks until the tasks it has awoken either complete their `async_access`
    //     /// or have returned `Poll::Pending` for a variety of reasons.
    //     /// The performance implications of this call are entirely dependent on the async runtime
    //     /// you are using it with, certain poor implementations *could* cause this to take longer
    //     /// than expect to resolve.
    //     /// Returns `Some` as long as the last call processed any number of waiting `async_access` calls.
    #[inline]
    fn tick_async_tasks(
        &self,
        system_set: InternedSystemSet,
        world: &mut World,
    ) -> TickAsyncTasksResult {
        let mut ecs_tasks = bevy_platform::prelude::vec![];
        while let Ok(mut async_system_state) =
            self.async_system_states.get_or_create(&system_set).pop()
        {
            async_system_state.initialize(world);
            ecs_tasks.push(async_system_state);
        }
        if ecs_tasks.is_empty() {
            return TickAsyncTasksResult::NoMoreTasksToTick;
        }
        let need_to_apply_system_state = self
            .world_access
            .scope(world, || wait_for_async_tasks(ecs_tasks));
        for task in need_to_apply_system_state {
            task.apply_system_params(world);
        }
        TickAsyncTasksResult::MoreTasksToTick
    }
}

struct AsyncSystemState {
    system_state_handler: Arc<dyn SystemStateHandler>,
    waker: Waker,
    wake_signal: WakeSignal,
    initialized: bool,
}

struct AwokenAsyncSystemState {
    system_state_handler: Arc<dyn SystemStateHandler>,
    wake_signal: WakeSignal,
}

struct NeedToApplyAsyncSystemState {
    system_state_handler: Arc<dyn SystemStateHandler>,
}

impl AsyncSystemState {
    #[inline]
    fn initialize(&mut self, world: &mut World) {
        if self.initialized {
            return;
        }
        self.system_state_handler.system_init(world);
        self.initialized = true;
    }
}

impl NeedToApplyAsyncSystemState {
    #[inline]
    fn apply_system_params(self, world: &mut World) {
        self.system_state_handler.system_apply(world);
    }
}

#[inline]
fn wait_for_async_tasks(ecs_tasks: Vec<AsyncSystemState>) -> Vec<NeedToApplyAsyncSystemState> {
    let ecs_tasks = ecs_tasks
        .into_iter()
        .map(
            |AsyncSystemState {
                 system_state_handler,
                 waker,
                 wake_signal,
                 ..
             }| {
                waker.wake();
                AwokenAsyncSystemState {
                    system_state_handler,
                    wake_signal,
                }
            },
        )
        // we re-collect to ensure we fully exhaust the prior iterator
        // we want to have all the wakers call .wake() before the first barrier calls .wait()
        .collect::<Vec<_>>();

    bevy_tasks::cfg::web! {
        if {} else {
            bevy_tasks::tick_global_task_pools_on_main_thread();
        }
    }

    ecs_tasks
        .into_iter()
        .map(
            |AwokenAsyncSystemState {
                 system_state_handler,
                 wake_signal,
             }| {
                wake_signal.wait();
                NeedToApplyAsyncSystemState {
                    system_state_handler,
                }
            },
        )
        .collect()
}

impl<P: SystemParam + 'static> EcsTask<P> {
    /// Allows you to access the ECS from any arbitrary async runtime.
    #[inline]
    pub async fn run_system<Func, Out, T: 'static>(
        &self,
        _sync_point: T,
        ecs_access: Func,
    ) -> Result<Out, AsyncEcsError>
    where
        for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
    {
        PendingEcsCall::<P, Func, Out> {
            phantom_data: Default::default(),
            ecs_func: Some(ecs_access),
            async_ecs: self.async_ecs.clone(),
            system_set: async_sync_point::<T>.into_system_set().intern(),
            barrier: None,
            system_state_handler: self.system_state_handler.clone(),
        }
        .await
    }
}

impl AsyncEcs {
    /// Creates a new `EcsTask` with `P` `SystemParam` that can be cloned and re-referenced to
    /// persist system parameters like `Changed`, `Added` or `Local`.
    #[inline]
    pub fn ecs_task<P: SystemParam + 'static>(&self) -> EcsTask<P> {
        EcsTask {
            phantom_data: Default::default(),
            async_ecs: Arc::downgrade(&self.0),
            system_state_handler: Arc::new(SystemStateHandlerStruct::<P>(Mutex::new(None))),
        }
    }
}

#[derive(PartialOrd, PartialEq, Eq, Ord, Hash, Debug, Copy, Clone)]
enum FutureState {
    Initialized,
    Uninitialized,
}

struct PendingEcsCall<P: SystemParam + 'static, Func, Out> {
    phantom_data: PhantomData<(P, Out)>,
    ecs_func: Option<Func>,
    async_ecs: Weak<AsyncEcsInternal>,
    system_set: InternedSystemSet,
    barrier: Option<WakeSignal>,
    system_state_handler: Arc<dyn SystemStateHandler>,
}

/// An `EcsTask` can be re-used in order to persist `SystemParams` like `Local`, `Changed`, or
/// `Added`
pub struct EcsTask<P: SystemParam + 'static> {
    phantom_data: PhantomData<P>,
    async_ecs: Weak<AsyncEcsInternal>,
    system_state_handler: Arc<dyn SystemStateHandler>,
}

impl<P: SystemParam + 'static> Clone for EcsTask<P> {
    fn clone(&self) -> Self {
        Self {
            phantom_data: Default::default(),
            async_ecs: self.async_ecs.clone(),
            system_state_handler: self.system_state_handler.clone(),
        }
    }
}

trait SystemStateHandler: Send + Sync + Any + 'static {
    fn system_init(&self, world: &mut World);

    fn system_apply(&self, world: &mut World);

    fn future_state(&self) -> FutureState;
}

struct SystemStateHandlerStruct<P: SystemParam + 'static>(Mutex<Option<SystemState<P>>>);

impl<P: SystemParam + 'static> SystemStateHandler for SystemStateHandlerStruct<P> {
    fn system_init(&self, world: &mut World) {
        let mut maybe_system_state = self.0.lock().unwrap();
        if maybe_system_state.is_some() {
            return;
        }
        maybe_system_state.replace(SystemState::<P>::new(world));
    }
    fn system_apply(&self, world: &mut World) {
        self.0.lock().unwrap().as_mut().unwrap().apply(world);
    }

    fn future_state(&self) -> FutureState {
        match self.0.try_lock() {
            Err(_) => FutureState::Initialized,
            Ok(value) => match value.is_some() {
                true => FutureState::Initialized,
                false => FutureState::Uninitialized,
            },
        }
    }
}

impl dyn SystemStateHandler {
    fn try_lock<P: SystemParam + 'static>(&self) -> Option<MutexGuard<Option<SystemState<P>>>> {
        (self as &dyn Any)
            .downcast_ref::<SystemStateHandlerStruct<P>>()
            .unwrap()
            .0
            .try_lock()
            .ok()
    }
}

#[derive(Error, Debug)]
pub enum AsyncEcsError {
    #[error(transparent)]
    SystemParamValidation(SystemParamValidationError),
    #[error("World no longer exists")]
    WorldNoLongerExists,
}

impl<P: SystemParam + 'static, Func, Out> Unpin for PendingEcsCall<P, Func, Out> {}

impl<P, Func, Out> Future for PendingEcsCall<P, Func, Out>
where
    P: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
{
    type Output = Result<Out, AsyncEcsError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _drop_at_end_of_scope = self.barrier.take();
        let async_ecs = match self.async_ecs.upgrade() {
            None => {
                return Poll::Ready(Err(AsyncEcsError::WorldNoLongerExists));
            }
            Some(async_ecs) => async_ecs,
        };
        match async_ecs
            .world_access
            .try_with(|world| {
                let world = world.as_unsafe_world_cell();
                let system_state_handler = self.system_state_handler.clone();
                let Some(mut system_state_guard) = system_state_handler.try_lock::<P>() else {
                    return Poll::Pending;
                };
                let Some(mut system_state) = system_state_guard.as_mut() else {
                    return Poll::Pending;
                };
                if !system_state.meta().is_send() {
                    return Poll::Ready(Err(AsyncEcsError::SystemParamValidation(
                        SystemParamValidationError::invalid::<NonSend<()>>(
                            "Cannot have your system be non-send / exclusive",
                        ),
                    )));
                }
                let state = match unsafe { system_state.get_unchecked(world) } {
                    Ok(state) => state,
                    Err(system_param_validation_error) => {
                        return Poll::Ready(Err(AsyncEcsError::SystemParamValidation(
                            system_param_validation_error,
                        )))
                    }
                };
                Poll::Ready(Ok(self.ecs_func.take().unwrap()(state)))
            })
            .ok()
        {
            Some(out) => out,
            None => {
                let wait_barrier = WakeSignal::new();
                self.barrier.replace(wait_barrier.clone());
                async_ecs
                    .async_system_states
                    .try_send(
                        &self.system_set,
                        AsyncSystemState {
                            system_state_handler: self.system_state_handler.clone(),
                            waker: cx.waker().clone(),
                            wake_signal: wait_barrier,
                            initialized: self.system_state_handler.future_state()
                                == FutureState::Initialized,
                        },
                    )
                    .ok()
                    .unwrap();
                Poll::Pending
            }
        }
    }
}
