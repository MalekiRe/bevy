use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use bevy_platform::sync::{Mutex, MutexGuard, OnceLock};
use core::any::Any;

/// Typed [`SystemState<Param>`] behind `OnceLock<Mutex>`. Initialization is deferred until
/// first used with the `&mut World`.
pub(crate) struct SystemStateCell<Param: SystemParam + 'static>(
    OnceLock<Mutex<SystemState<Param>>>,
);

impl<Param: SystemParam + 'static> Default for SystemStateCell<Param> {
    fn default() -> Self {
        Self(OnceLock::default())
    }
}

/// Type-erased interface for `SystemStateCell`, so that they may be stored
/// uniformly as `Arc<dyn ErasedSystemStateCell>`.
pub(crate) trait ErasedSystemStateCell: Send + Sync + Any + 'static {
    fn apply(&self, world: &mut World);
}

impl<Param: SystemParam + 'static> ErasedSystemStateCell for SystemStateCell<Param> {
    fn apply(&self, world: &mut World) {
        self.0
            .get()
            // Invariant: always initialized before apply is called.
            .unwrap()
            .lock()
            .unwrap()
            .apply(world);
    }
}

impl dyn ErasedSystemStateCell {
    /// Attempt to access the inner `SystemState<Param>` with the given `&mut World`, lazily
    /// initializing it if necessary. Returns `None` if the lock can't be acquired.
    pub(crate) fn try_lock<'w, 'a, Param: SystemParam + 'static>(
        &'a self,
        world: &'w mut World,
    ) -> Option<MutexGuard<'a, SystemState<Param>>>
    where
        'a: 'w,
    {
        (self as &dyn Any)
            .downcast_ref::<SystemStateCell<Param>>()
            // Invariant: caller must use the same `Param` that created this cell.
            .unwrap()
            .0
            .get_or_init(|| Mutex::new(SystemState::new(world)))
            // Non-blocking: contended callers requeue rather than stall.
            .try_lock()
            .ok()
    }
}
