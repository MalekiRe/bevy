use crate::ecs_access::AsyncSystemHandle;
use crate::system_state_store::TypedStateStore;
use bevy_app::App;
use bevy_ecs::system::SystemParam;
use std::marker::PhantomData;
use std::sync::Arc;

/// Plugin entry point for the async <-> ECS bridge system.
///
/// This plugin installs a configuration resource telling the bridge how aggressively to drive work
/// at each sync point.
///
/// Conceptually, async tasks cannot directly access Bevy ECS state from arbitrary
/// threads or arbitrary times. Instead, they enqueue requests which are later
/// driven from a known ECS `SyncPoint` on the world-owning thread.
///
/// This supports arbitrary async runtimes as well as multiple Bevy Worlds / Bevy Apps.
pub struct AsyncPlugin {
    /// Upper bound on how many internal bridge ticks we perform each time a
    /// sync point system runs.
    ///
    /// A single "bridge tick" means:
    /// 1. collect queued access requests for that sync point,
    /// 2. wake the corresponding async tasks,
    /// 3. wait for each one to attempt a poll,
    /// 4. apply any deferred `SystemState` work back into the world.
    ///
    /// We may need to do this multiple times because one task's progress can
    /// unblock another task that previously returned `Poll::Pending`.
    pub tick_budget: usize,
}

impl Default for AsyncPlugin {
    fn default() -> Self {
        Self {
            tick_budget: 100,
        }
    }
}

impl bevy_app::Plugin for AsyncPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AsyncTickBudget(
            self.tick_budget,
        ))
        .init_resource::<AsyncBridge>();
    }
}

/// Internal resource to manage a limit on how many times we try to drive the async <-> ecs bridge
/// per sync point.
#[derive(bevy_ecs_macros::Resource, Clone)]
pub(crate) struct AsyncTickBudget(pub(crate) usize);

/// This resource gives one the ability to bridge a connection between an async task and the ecs.
/// By calling `AsyncBridge::create_handle(&self)` you create a new bridge handle between an async task
/// and the ecs.
#[derive(bevy_ecs_macros::Resource, Default, Clone)]
pub struct AsyncBridge(pub(crate) Arc<crate::async_bridge::BridgeState>);

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
            system_state: Arc::new(TypedStateStore::<P>::default()),
        }
    }
}
