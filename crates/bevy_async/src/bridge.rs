use crate::plugin::AsyncTickBudget;
use crate::request::RequestQueues;
use crate::system_state_cell::SystemStateCell;
use crate::AsyncSystemHandle;
use bevy_ecs::prelude::{IntoSystemSet, SystemSet, World};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::SystemParam;
use bevy_platform::sync::Arc;
use core::marker::PhantomData;

/// This resource gives one the ability to bridge a connection between an async task and the ecs.
/// By calling `AsyncBridge::create_handle(&self)` you create a new bridge handle between an async task
/// and the ecs.
#[derive(bevy_ecs_macros::Resource, Default, Clone)]
pub struct AsyncBridge(pub(crate) Arc<BridgeState>);

impl AsyncBridge {
    /// Creates a reusable async handle for accessing the ECS with the
    /// `SystemParam` type `P`.
    ///
    /// This is the entry-point to let an
    /// async task interact with Bevy ECS state.
    ///
    /// The returned [`AsyncSystemHandle<P>`]:
    /// - is cheap to clone,
    /// - can be moved into async tasks,
    /// - does not access the world immediately,
    /// [`AsyncSystemHandle<P>`] waits until a matching sync point drives the bridge and
    ///   temporarily grants safe ECS access.
    ///
    /// You create one of these from a cloned [`AsyncBridge`] resource and
    /// then call `.run(...)` inside async code whenever you want to access the ECS.
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
    /// `P` is stored lazily, meaning the underlying `SystemState<P>` is only
    /// initialized when the bridge is first driven against a real `World`.
    pub fn create_handle<P: SystemParam + 'static>(&self) -> AsyncSystemHandle<P> {
        AsyncSystemHandle {
            _p: PhantomData::default(),
            bridge: Arc::downgrade(&self.0),
            system_state: Arc::new(SystemStateCell::<P>::default()),
        }
    }
}

#[derive(Default)]
pub(crate) struct BridgeState {
    pub(crate) request_queues: RequestQueues,
    pub(crate) scoped_world: scoped_static_storage::ScopedStatic<World>,
}

impl BridgeState {
    /// This drives a single sync point, requesting the poll of all tasks in that sync point.
    /// None of the tasks are guaranteed to actually return `Poll::Ready`, but all are guaranteed to
    /// at least do a `Poll::Pending`
    ///
    /// The flow of logic is the following:
    /// 1. We first drain the queue for our `SyncPoint` into a batch of requests.
    ///    In the process, we initialize each request's `SystemState`. (This is idempotent).
    /// 2. If the batch is empty, we return early.
    /// 3. Expose our `World` through `scoped_world`.
    /// 4. Wake all our `AsyncSystemHandleFut`s.
    /// 5. Wait for each Fut to poll at least once.
    /// 6. Apply the Fut's `SystemState` back into the `World`. (Things like `Commands`).
    fn tick_sync_point(&self, sync_point_key: InternedSystemSet, world: &mut World) -> TickResult {
        let batch = self.request_queues.drain_queue(sync_point_key, world);

        // If no requests were waiting then report idle so the caller can decide whether to stop
        // or attempt one more task-pool tick.
        if batch.is_empty() {
            return TickResult::NoWork;
        }

        // Make this `World` temporarily visible to our waking futures. Wake them all and wait
        // until they all have at least *attempted* to poll.
        // This is contractually obligated by the contract of `.wake()`. We are guaranteed one wake
        // per call to our `.wake()`.
        let polled_requests = self.scoped_world.scope(world, || {
            let woken_tasks = batch.wake_all();

            #[cfg(feature = "bevy_tasks")]
            bevy_tasks::cfg::web! {
                if {} else {
                    bevy_tasks::tick_global_task_pools_on_main_thread();
                }
            }

            woken_tasks.wait_all()
        });

        polled_requests.apply(world);
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
