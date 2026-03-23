use crate::bridge::AsyncBridge;
use bevy_app::App;

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
        Self { tick_budget: 100 }
    }
}

impl bevy_app::Plugin for AsyncPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AsyncTickBudget(self.tick_budget))
            .init_resource::<AsyncBridge>();
    }
}

/// Internal resource to manage a limit on how many times we try to drive the async <-> ecs bridge
/// per sync point.
#[derive(bevy_ecs_macros::Resource, Clone)]
pub(crate) struct AsyncTickBudget(pub(crate) usize);
