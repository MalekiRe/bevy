use bevy_app::{App, Plugin};

pub struct AsyncEcsPlugin {
    emergency_exit_amount: usize,
}

impl Default for AsyncEcsPlugin {
    fn default() -> Self {
        Self {
            emergency_exit_amount: 100,
        }
    }
}

impl Plugin for AsyncEcsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<bevy_ecs::prelude::AsyncEcs>();
        app.insert_resource(bevy_ecs::prelude::EmergencyExitAmount(
            self.emergency_exit_amount,
        ));
    }
}
