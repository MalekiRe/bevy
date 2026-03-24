use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use bevy_platform::sync::{ConditionalSend, Mutex, MutexGuard, OnceLock};

/// Typed `SystemState<Params>` behind `OnceLock<Mutex>`, erased via `dyn ErasedSystemStateCell`
/// so the bridge can store heterogeneous params. Initialization is deferred until
/// the first drive on the world-owning thread (requires `&mut World`).
pub(crate) struct SystemStateCell<Params: SystemParam + 'static> {
    inner: OnceLock<Mutex<SystemState<Params>>>,
}

impl<Params: SystemParam + 'static> Default for SystemStateCell<Params> {
    fn default() -> Self {
        Self {
            inner: OnceLock::default(),
        }
    }
}

/// Type-erased interface for `SystemStateCell`, letting the bridge store
/// request state uniformly as `Arc<dyn ErasedSystemStateCell>`.
pub(crate) trait ErasedSystemStateCell:
    ConditionalSend + Sync + core::any::Any + 'static
{
    /// Lazily initialize the `SystemState`. Idempotent; requires `&mut World`.
    fn ensure_initialized(&self, world: &mut World);

    /// Apply deferred operations (e.g. `Commands` buffers) back into the world.
    fn apply(&self, world: &mut World);
}

impl<Params: SystemParam + 'static> ErasedSystemStateCell for SystemStateCell<Params> {
    fn ensure_initialized(&self, world: &mut World) {
        self.inner
            .get_or_init(|| Mutex::new(SystemState::new(world)));
    }

    fn apply(&self, world: &mut World) {
        self.inner
            .get()
            // Invariant: always initialized before apply is called.
            .unwrap()
            .lock()
            .unwrap()
            .apply(world);
    }
}

impl dyn ErasedSystemStateCell {
    pub(crate) fn try_lock<Params: SystemParam + 'static>(
        &self,
    ) -> Option<MutexGuard<'_, SystemState<Params>>> {
        (self as &dyn core::any::Any)
            .downcast_ref::<SystemStateCell<Params>>()
            // Caller must use the same `Params` that created this cell.
            .unwrap()
            .inner
            .get()?
            // Non-blocking: contended callers requeue rather than stall.
            .try_lock()
            .ok()
    }
}
