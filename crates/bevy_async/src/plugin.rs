use crate::world::{AsyncWorld, StrongAsyncWorld};
use bevy_app::App;
use bevy_platform::sync::Arc;

/// Plugin that installs the [`AsyncWorld`] resource and configures
/// how aggressively sync points drive queued tasks.
pub struct AsyncPlugin {
    /// Max internal ticks per sync point. Higher values let chained
    /// `.await` calls complete in a single frame at the cost of longer system runs.
    pub max_async_ticks_per_sync_point: usize,
}

impl Default for AsyncPlugin {
    fn default() -> Self {
        Self {
            max_async_ticks_per_sync_point: 100,
        }
    }
}

impl bevy_app::Plugin for AsyncPlugin {
    fn build(&self, app: &mut App) {
        let strong_world = StrongAsyncWorld::default();
        let weak_world = AsyncWorld(Arc::downgrade(&strong_world.0));
        app.insert_resource(AsyncTickBudget(self.max_async_ticks_per_sync_point))
            .insert_resource(strong_world)
            .insert_resource(weak_world);
    }
}

#[derive(bevy_ecs_macros::Resource, Clone)]
pub(crate) struct AsyncTickBudget(pub(crate) usize);
