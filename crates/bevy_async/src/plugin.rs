use crate::bridge::AsyncBridge;
use bevy_app::App;

/// Plugin that installs the [`AsyncBridge`] resource and configures
/// how aggressively sync points drive queued work.
pub struct AsyncPlugin {
    /// Max internal ticks per sync point. Higher values let chained
    /// `.await` calls complete in a single frame at the cost of longer system runs.
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
