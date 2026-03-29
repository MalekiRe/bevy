use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use bevy_platform::sync::{Mutex, MutexGuard, OnceLock};
use core::any::Any;

pub(crate) struct SystemStateCell<Param: SystemParam + 'static>(OnceLock<Mutex<SystemState<Param>>>);

impl<Param: SystemParam + 'static> Default for SystemStateCell<Param> {
    fn default() -> Self {
        Self(OnceLock::default())
    }
}

pub(crate) trait ErasedSystemStateCell: Send + Sync + Any + 'static {
    fn apply(&self, world: &mut World);
}

impl<Param: SystemParam> ErasedSystemStateCell for SystemStateCell<Param> {
    fn apply(&self, world: &mut World) {
        self.0.get().unwrap().lock().unwrap().apply(world);
    }
}

impl dyn ErasedSystemStateCell {
    pub(crate) fn try_lock<'w, 'a, Param: SystemParam + 'static>(
        &'a self,
        world: &'w mut World,
    ) -> Option<MutexGuard<'a, SystemState<Param>>>
    where
        'a: 'w,
    {
        (self as &dyn Any)
            .downcast_ref::<SystemStateCell<Param>>()
            .unwrap()
            .0
            .get_or_init(|| Mutex::new(SystemState::new(world)))
            .try_lock()
            .ok()
    }
}
