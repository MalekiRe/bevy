use crate::job::{ErasedJob, JobFut};
use crate::plugin::AsyncTickBudget;
use crate::system_state_cell::{ErasedSystemStateCell, SystemStateCell};
use crate::AsyncAccessError;
use bevy_ecs::prelude::{IntoSystemSet, SystemSet, World};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_platform::sync::Weak;
use bevy_platform::sync::{Arc, ConditionalSend};
use core::marker::PhantomData;
use keyed_concurrent_queue::KeyedQueues;

#[cfg(feature = "std")]
use crate::scoped::{ScopedFut, ScopedRequest};

/// Shared resource for creating [`AsyncParams`] handles that let async tasks access the ECS.
#[derive(bevy_ecs_macros::Resource, Default, Clone)]
pub struct AsyncBridge(pub(crate) Arc<BridgeState>);

impl AsyncBridge {
    /// Creates a reusable, cloneable handle for accessing `Params` from async code.
    /// Call [`.run(...)`](AsyncParams::run) on the returned handle to access the ECS.
    ///
    /// # Example
    /// ```rust
    /// use bevy_app::prelude::*;
    /// use bevy_async::prelude::*;
    /// use bevy_ecs::prelude::*;
    /// use bevy_tasks::AsyncComputeTaskPool;
    /// use bevy_platform::sync::atomic::AtomicBool;
    /// use bevy_platform::sync::atomic::Ordering;
    /// use bevy_platform::sync::Arc;
    /// use bevy_app::ScheduleRunnerPlugin;
    ///
    /// struct MySyncPoint;
    /// static ACCESS_RAN: AtomicBool = AtomicBool::new(false);
    /// fn main() {
    ///   let mut app = App::new();
    ///   app.add_plugins((AsyncPlugin::default(), ScheduleRunnerPlugin::default(), TaskPoolPlugin::default()));
    ///   app.add_systems(Update, tick_async_bridge::<MySyncPoint>);
    ///   app.add_systems(Startup, move |bridge: Res<AsyncBridge>| {
    ///       let bridge = bridge.clone();
    ///       AsyncComputeTaskPool::get().spawn(async move {
    ///           let bridge_handle = bridge.create_handle::<Commands>();
    ///           bridge_handle.run(MySyncPoint, |mut commands: Commands| {
    ///               commands.spawn_empty();
    ///               ACCESS_RAN.store(true, Ordering::Relaxed);
    ///           }).await.unwrap();
    ///       }).detach();
    ///   });
    ///   app.update();
    ///
    ///   assert!(ACCESS_RAN.load(Ordering::Relaxed));
    /// }
    ///
    /// ```
    ///
    /// The underlying `SystemState<Params>` is initialized lazily on first use.
    pub fn create_handle<Params: SystemParam + 'static>(&self) -> AsyncParams<Params> {
        AsyncParams {
            _p: PhantomData::default(),
            bridge: Arc::downgrade(&self.0),
            system_state: Arc::new(SystemStateCell::<Params>::default()),
        }
    }
}

/// Cloneable handle that lets async tasks access an ECS `SystemParam`.
/// Multiple tasks sharing the same handle will share `Locals` and filter state.
pub struct AsyncParams<Params: SystemParam + 'static> {
    pub(crate) _p: PhantomData<Params>,

    /// Weak so access fails gracefully with [`AsyncAccessError::WorldDropped`].
    pub(crate) bridge: Weak<BridgeState>,

    /// Reused across accesses to persist `Local`s and change-detection state.
    pub(crate) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl<Params: SystemParam + 'static> Clone for AsyncParams<Params> {
    fn clone(&self) -> Self {
        Self {
            _p: PhantomData::default(),
            bridge: self.bridge.clone(),
            system_state: self.system_state.clone(),
        }
    }
}

impl<Params: SystemParam + 'static> AsyncParams<Params> {
    /// Queues `world_fn` to run at the given sync point. Dropping the future cancels the job.
    pub async fn run<Func, Out, SyncPoint: 'static>(
        &self,
        _sync_point: SyncPoint,
        world_fn: Func,
    ) -> Result<Out, AsyncAccessError>
    where
        for<'w, 's> Func: FnOnce(Params::Item<'w, 's>) -> Out + 'static,
        Func: ConditionalSend,
        Out: ConditionalSend + 'static,
    {
        let sync_point_key = tick_async_bridge::<SyncPoint>.into_system_set().intern();
        JobFut::new(sync_point_key, world_fn, &self).await
    }

    /// Like [`run`](Self::run), but the closure does not need to be `'static` or `Send`.
    /// Only available with `std` (relies on `Condvar` for the blocking handshake).
    #[cfg(feature = "std")]
    pub async fn run_scoped<Func, Out, SyncPoint: 'static>(
        &self,
        _sync_point: SyncPoint,
        world_fn: Func,
    ) -> Result<Out, AsyncAccessError>
    where
        for<'w, 's> Func: FnOnce(Params::Item<'w, 's>) -> Out,
    {
        let sync_point_key = tick_async_bridge::<SyncPoint>.into_system_set().intern();
        ScopedFut::new(sync_point_key, world_fn, &self).await
    }
}

pub(crate) struct BridgeState {
    pub(crate) job_queues: KeyedQueues<InternedSystemSet, Arc<dyn ErasedJob>>,
    #[cfg(feature = "std")]
    pub(crate) scoped_request_queues: KeyedQueues<InternedSystemSet, ScopedRequest>,
    #[cfg(feature = "std")]
    pub(crate) scoped_world: scoped_static_storage::ScopedStatic<World>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            job_queues: KeyedQueues::default(),
            #[cfg(feature = "std")]
            scoped_request_queues: KeyedQueues::default(),
            #[cfg(feature = "std")]
            scoped_world: scoped_static_storage::ScopedStatic::new(),
        }
    }
}

impl BridgeState {
    /// Drives one sync point, returning whether any work was done.
    fn tick_sync_point(&self, sync_point_key: InternedSystemSet, world: &mut World) -> TickResult {
        let job_queue = self.job_queues.get_or_create(&sync_point_key);
        let mut total = 0;
        total += crate::job::tick_job_queue(&job_queue, world);

        #[cfg(feature = "std")]
        {
            let scoped_queue = self.scoped_request_queues.get_or_create(&sync_point_key);
            total += crate::scoped::tick_scoped_queue(&scoped_queue, &self.scoped_world, world);
        }

        if total > 0 {
            TickResult::DidWork
        } else {
            TickResult::NoWork
        }
    }
}

/// Whether a tick attempt did any work.
#[derive(PartialEq)]
enum TickResult {
    /// At least one job or scoped request was processed.
    DidWork,
    /// There was no queued work available for the `SyncPoint`.
    NoWork,
}

/// System that drives queued bridge work for `SyncPoint`.
///
/// Ticks up to [`AsyncTickBudget`] times so that chained `.await` calls
/// can complete within a single frame. Also retries jobs whose `SystemState`
/// lock was contended.
pub fn tick_async_bridge<SyncPoint: 'static>(world: &mut World) {
    let sync_point_key = tick_async_bridge::<SyncPoint>.into_system_set().intern();
    let bridge = world.get_resource::<AsyncBridge>().unwrap().clone();
    let max_ticks = world.get_resource::<AsyncTickBudget>().unwrap().0;
    for _ in 0..max_ticks {
        if bridge.0.tick_sync_point(sync_point_key, world) == TickResult::NoWork {
            #[cfg(feature = "bevy_tasks")]
            bevy_tasks::cfg::web! {
                if {
                    return;
                } else {
                    bevy_tasks::tick_global_task_pools_on_main_thread();
                    // Retry once after ticking the global pool.
                    if bridge.0.tick_sync_point(sync_point_key, world) == TickResult::NoWork {
                        return;
                    }
                }
            }
            #[cfg(not(feature = "bevy_tasks"))]
            return;
        }
    }
}
