use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use bevy_platform::sync::{Mutex, MutexGuard, OnceLock};

/// Stores a typed `SystemState<P>` behind a `OnceLock<Mutex>` so it can be initialized once
/// on the world-owning thread and then shared across bridge requests.
///
/// Why this exists:
/// `SystemState<P>` is typed, but the bridge queue needs to store heterogeneous
/// requests without knowing `P` at compile time. So each concrete
/// `SystemStateCell<P>` is later erased behind `dyn ErasedSystemStateCell`.
///
/// The `OnceLock` starts empty because we cannot construct `SystemState<P>` until
/// we have a mutable `World`. Initialization is deferred to the first time the
/// request is driven on the world-owning thread.
pub(crate) struct SystemStateCell<P: SystemParam + 'static> {
    inner: OnceLock<Mutex<SystemState<P>>>,
}

impl<P: SystemParam + 'static> Default for SystemStateCell<P> {
    fn default() -> Self {
        Self {
            inner: OnceLock::default(),
        }
    }
}

/// Allows us to erase the `SystemStateCell` so we can pass it to and from the ecs.
///
/// This lets the bridge store all request state uniformly as `Arc<dyn ErasedSystemStateCell>`.
///
/// This trait exposes the following operations:
/// - initialize the typed `SystemState` if needed,
/// - apply deferred state back into the world.
pub(crate) trait ErasedSystemStateCell: Send + Sync + core::any::Any + 'static {
    /// Lazily initialize the underlying typed `SystemState`.
    ///
    /// Idempotent. Must run on the world-owning thread because `SystemState::new`
    /// requires `&mut World`.
    fn ensure_initialized(&self, world: &mut World);

    /// Apply deferred operations accumulated by the `SystemState` back into
    /// the world.
    ///
    /// For example, `Commands` buffers are typically flushed during `apply`.
    fn apply(&self, world: &mut World);
}

impl<P: SystemParam> ErasedSystemStateCell for SystemStateCell<P> {
    fn ensure_initialized(&self, world: &mut World) {
        self.inner
            .get_or_init(|| Mutex::new(SystemState::new(world)));
    }

    fn apply(&self, world: &mut World) {
        self.inner
            .get()
            // We expect initialization to have already occurred before `apply` is
            // ever called. So `unwrap()` here reflects an invariant of the bridge.
            // Completed requests only exist for initialized system states.
            .unwrap()
            .lock()
            .unwrap()
            .apply(world);
    }
}

impl dyn ErasedSystemStateCell {
    pub(crate) fn try_lock<P: SystemParam + 'static>(
        &self,
    ) -> Option<MutexGuard<'_, SystemState<P>>> {
        // Recover the concrete typed cell from the erased trait object.
        (self as &dyn core::any::Any)
            .downcast_ref::<SystemStateCell<P>>()
            // This `unwrap()` encodes another invariant of the design, it is the case that every
            // call site must ask for the same `P` that was originally used to create the erased cell.
            // A mismatch here would be a logic bug in the bridge, and should never ever happen.
            .unwrap()
            .inner
            .get()? // fail if not initialized
            // Use `try_lock` rather than blocking:
            // if another request currently owns the typed `SystemState<P>`, the
            // caller should yield with `Poll::Pending` instead of stalling a
            // thread. We get ticked optimistically many times so it's okay. We can simply
            // requeue if we can't acquire the lock, instead of blocking an async task
            // which would be very bad.
            .try_lock()
            .ok()
    }
}
