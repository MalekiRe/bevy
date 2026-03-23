use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use bevy_platform::sync::{atomic::AtomicBool, Mutex, MutexGuard};

/// Stores a typed `SystemState<P>` behind a mutex so it can be initialized once
/// and then shared across bridge requests.
///
/// Why this exists:
/// `SystemState<P>` is typed, but the bridge queue needs to store heterogeneous
/// requests without knowing `P` at compile time. So each concrete
/// `TypedStateStore<P>` is later erased behind `dyn ErasedStateStore`.
///
/// The inner `Option` starts as `None` because we cannot construct the
/// `SystemState<P>` until we have a mutable `World`. Furthermore, it is not safe to try to
/// initialize the `SystemState<P>` from a thread *other* than the world-owning thread, so
/// we have to start it as none and have the initialization occur on the world-owning thread before
/// the `SystemState<P>` is ever used.
pub(crate) struct TypedStateStore<P: SystemParam + 'static> {
    inner: Mutex<Option<SystemState<P>>>,
    ready: AtomicBool,
}

impl<P: SystemParam + 'static> Default for TypedStateStore<P> {
    fn default() -> Self {
        // Start uninitialized. Initialization is deferred until the request is
        // first driven on the world-owning thread with access to `&mut World`.
        Self {
            inner: Mutex::new(None),
            ready: AtomicBool::new(false)
        }
    }
}

/// Allows us to erase the `TypedStateStore` so we can pass it to and from the ecs.
///
/// This lets the bridge store all request state uniformly as `Arc<dyn ErasedStateStore>`.
///
/// This trait exposes the following operations:
/// - initialize the typed `SystemState` if needed,
/// - apply deferred state back into the world,
/// - ask whether initialization has already happened.
pub(crate) trait ErasedStateStore: Send + Sync + core::any::Any + 'static {
    /// Lazily initialize the underlying typed `SystemState`.
    ///
    /// If initialization has already happened, this is idempodent, however it is not 0-cost because
    /// of the forced pointer chasing and indirection of a `dyn Trait`.
    ///
    /// This must run on the world-owning thread because `SystemState::new`
    /// requires `&mut World`.
    fn initialize(&self, world: &mut World);

    /// Apply deferred operations accumulated by the `SystemState` back into
    /// the world.
    ///
    /// For example, `Commands` buffers are typically flushed during `apply`.
    fn apply(&self, world: &mut World);

    /// Returns `true` if the system is initialized, `false` if it is uninitialized.
    fn is_initialized(&self) -> bool;
}

impl<P: SystemParam> ErasedStateStore for TypedStateStore<P> {
    fn initialize(&self, world: &mut World) {
        // Lock the store so only one thread/driver path can perform the lazy
        // initialization.
        let mut system_state = self.inner.lock().unwrap();
        // If another earlier request already initialized the state we are done.
        if system_state.is_some() {
            return;
        }
        system_state.replace(SystemState::new(world));
        self.ready
            .store(true, bevy_platform::sync::atomic::Ordering::Release);
    }

    fn apply(&self, world: &mut World) {
        // We expect initialization to have already occurred before `apply` is
        // ever called. So `unwrap()` here reflects an invariant of the bridge.
        // Completed requests only exist for initialized system states.
        self.inner.lock().unwrap().as_mut().unwrap().apply(world);
    }

    fn is_initialized(&self) -> bool {
        // If the atomic bool *says* it's loaded then we know for sure it is.
        // Otherwise we have to conservatively assume it's not initialized.
        // This is okay because our initialization logic is idempotent.
        self.ready.load(bevy_platform::sync::atomic::Ordering::Acquire)
    }
}

impl dyn ErasedStateStore {
    pub(crate) fn try_lock<P: SystemParam + 'static>(
        &self,
    ) -> Option<MutexGuard<Option<SystemState<P>>>> {
        // Recover the concrete typed store from the erased trait object.
        //
        // This `unwrap()` encodes another invariant of the design, it is the case that every
        // call site must ask for the same `P` that was originally used to create the erased store.
        // A mismatch here would be a logic bug in the bridge, and should never ever happen.
        (self as &dyn core::any::Any)
            .downcast_ref::<TypedStateStore<P>>()
            .unwrap()
            .inner
            // Use `try_lock` rather than blocking:
            // if another request currently owns the typed `SystemState<P>`, the
            // caller should yield with `Poll::Pending` instead of stalling a
            // thread. We get ticked optimistically many times so it's okay. We aren't guaranteed to
            // run everytime so we can return Poll::Pending instead of blocking an async task
            // which would be very bad.
            .try_lock()
            .ok()
    }
}
