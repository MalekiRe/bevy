use crate::bridge::{BridgeFut, BridgeRequest};
use crate::plugin::AsyncTickBudget;
use crate::system_state::{ErasedSystemStateCell, SystemStateCell};
use crate::EcsAccessError;
use bevy_ecs::schedule::{InternedSystemSet, IntoSystemSet, SystemSet};
use bevy_ecs::system::SystemParam;
use bevy_ecs::world::World;
use bevy_platform::sync::{Arc, Weak};
use core::marker::PhantomData;
use keyed_concurrent_queue::KeyedQueues;
use scoped_static_storage::ScopedStatic;

#[derive(bevy_ecs_macros::Resource, Default, Clone)]
pub(crate) struct StrongAsyncWorld(pub(crate) Arc<AsyncWorldInner>);

/// Shared resource for creating [`AsyncSystemState`]s that let async tasks access the ECS.
#[derive(bevy_ecs_macros::Resource, Default, Clone)]
pub struct AsyncWorld(pub(crate) Weak<AsyncWorldInner>);

impl AsyncWorld {
    /// Creates a reusable, cloneable handle for accessing [`SystemParams`](bevy_ecs::system::SystemParam) from async code.
    /// Call [`.bridge(...)`](AsyncSystemState::bridge) on the returned handle to access the ECS.
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
    ///   app.add_systems(Update, async_world_sync_point::<MySyncPoint>);
    ///   app.add_systems(Startup, move |world: Res<AsyncWorld>| {
    ///       let world = world.clone();
    ///       AsyncComputeTaskPool::get().spawn(async move {
    ///           let commands_state = world.system_state::<Commands>();
    ///           commands_state.bridge(MySyncPoint, |mut commands| {
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
    /// The underlying [`SystemState<Param>`](bevy_ecs::system::SystemParam) is initialized lazily on first use.
    pub fn system_state<Param: SystemParam + 'static>(&self) -> AsyncSystemState<Param> {
        AsyncSystemState::new(self.clone())
    }
}

#[derive(Default)]
pub(crate) struct AsyncWorldInner {
    pub(crate) bridge_requests: KeyedQueues<InternedSystemSet, BridgeRequest>,
    pub(crate) world_scope: ScopedStatic<World>,
}

impl AsyncWorldInner {
    fn tick_sync_point(&self, sync_point: InternedSystemSet, world: &mut World) -> TickResult {
        let bridge_queue = self.bridge_requests.get_or_create(&sync_point);
        crate::bridge::tick_bridge_queue(&bridge_queue, &self.world_scope, world)
    }
}

/// Cloneable handle that lets async tasks access an ECS [`SystemParam`](bevy_ecs::system::SystemParam).
/// Multiple tasks sharing the same handle will share [`Local`](bevy_ecs::system::Local)s and other state.
pub struct AsyncSystemState<Param: SystemParam + 'static> {
    pub(crate) _p: PhantomData<Param>,
    pub(crate) inner: Arc<dyn ErasedSystemStateCell>,
    pub(crate) world: AsyncWorld,
}

impl<Param: SystemParam + 'static> Clone for AsyncSystemState<Param> {
    fn clone(&self) -> Self {
        Self {
            _p: PhantomData::default(),
            inner: self.inner.clone(),
            world: self.world.clone(),
        }
    }
}

impl<Param: SystemParam + 'static> AsyncSystemState<Param> {
    /// Creates a new [`AsyncSystemParam`] (see [`AsyncWorld::system_state`])
    pub fn new(world: AsyncWorld) -> Self {
        Self {
            _p: PhantomData::default(),
            inner: Arc::new(SystemStateCell::<Param>::default()),
            world,
        }
    }

    /// Queues `bridge_fn` to run at the given sync point. This future is cancel-safe.
    pub async fn bridge<BridgeFn, Out, SyncPoint: 'static>(
        &self,
        _sync_point: SyncPoint,
        bridge_fn: BridgeFn,
    ) -> Result<Out, EcsAccessError>
    where
        for<'w, 's> BridgeFn: FnOnce(Param::Item<'w, 's>) -> Out,
    {
        let sync_point_key = async_world_sync_point::<SyncPoint>
            .into_system_set()
            .intern();
        BridgeFut::new(sync_point_key, bridge_fn, &self).await
    }
}

#[derive(PartialEq)]
pub(crate) enum TickResult {
    DidWork,
    NoWork,
}

/// System that drives queued tasks for `SyncPoint`.
///
/// Ticks up to [`AsyncTickBudget`] times so that chained `.await` calls
/// can complete within a single frame. Retries tasks fairly until they all
/// get a chance to run.
pub fn async_world_sync_point<SyncPoint: 'static>(world: &mut World) {
    let sync_point = async_world_sync_point::<SyncPoint>
        .into_system_set()
        .intern();
    let strong_world = world.get_resource::<StrongAsyncWorld>().unwrap().clone();
    let max_ticks = world.get_resource::<AsyncTickBudget>().unwrap().0;
    for _ in 0..max_ticks {
        if strong_world.0.tick_sync_point(sync_point, world) == TickResult::NoWork {
            #[cfg(feature = "bevy_tasks")]
            bevy_tasks::cfg::web! {
                if {} else {
                    bevy_tasks::tick_global_task_pools_on_main_thread();
                }
            }

            if strong_world.0.tick_sync_point(sync_point, world) == TickResult::NoWork {
                return;
            }
        }
    }
}
