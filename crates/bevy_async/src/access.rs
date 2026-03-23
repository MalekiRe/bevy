use crate::bridge;
use crate::bridge::BridgeState;
use crate::guarded_latch::LatchGuard;
use crate::request::PendingRequest;
use crate::system_state_cell::ErasedSystemStateCell;
use bevy_ecs::prelude::World;
use bevy_ecs::schedule::{InternedSystemSet, IntoSystemSet, SystemSet};
use bevy_ecs::system::SystemParam;
use bevy_platform::sync::{Arc, Weak};
use core::marker::PhantomData;

/// Handle that lets an async task request temporary access to an ECS
/// `SystemParam` or a tuple of them.
///
/// `P` is the typed system parameter the caller eventually wants, such as:
/// - [`bevy_ecs::prelude::Commands`]
/// - [`bevy_ecs::prelude::Res`]
/// - [`bevy_ecs::prelude::Query`]
/// - tuples of params
///
/// It is cheap to clone and intended to be passed into async tasks.
/// You can pass it into *multiple* tasks on separate threads and have them work concurrently
/// off of the same state, sharing `Locals`.
pub struct AsyncSystemHandle<P: SystemParam + 'static> {
    pub(crate) _p: PhantomData<P>,

    /// A `Weak` is used so tasks do not stay alive if the world is dropped.
    /// If the world goes away, upgrading this weak pointer fails and access
    /// returns [`AsyncAccessError::WorldDropped`].
    pub(crate) bridge: Weak<BridgeState>,

    /// Type-erased storage for the underlying `SystemState<P>`.
    ///
    /// Each `AsyncSystemHandle<P>` keeps reusing the same typed system state across
    /// accesses so repeated operations do not rebuild it from scratch.
    ///
    /// This is also important not only to persist params like `Local` but *also* so `Changed` and
    /// `Added` and other filters can work.
    pub(crate) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl<P: SystemParam + 'static> Clone for AsyncSystemHandle<P> {
    fn clone(&self) -> Self {
        Self {
            _p: PhantomData::default(),
            bridge: self.bridge.clone(),
            system_state: self.system_state.clone(),
        }
    }
}

impl<P: SystemParam + 'static> AsyncSystemHandle<P> {
    pub async fn run<Func, Out, SyncPoint: 'static>(
        &self,
        _sync_point: SyncPoint,
        world_fn: Func,
    ) -> Result<Out, AsyncAccessError>
    where
        for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
    {
        let sync_point_key = bridge::tick_async_bridge::<SyncPoint>
            .into_system_set()
            .intern();

        let world_fn = WorldFn {
            _p: PhantomData::default(),
            func: Some(world_fn),
            system_state: self.system_state.clone(),
        };

        AsyncSystemHandleFut {
            sync_point_key,
            world_fn,
            maybe_poll_guard: None,
            bridge: self.bridge.clone(),
        }
        .await
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AsyncAccessError {
    /// The requested `SystemParam` was invalid in the current world context.
    /// for example trying to access a param that fails Bevy's usual validation like a missing
    /// Resource or using `Single` on something that has 0 or multiple instances.
    #[error(transparent)]
    InvalidParam(bevy_ecs::system::SystemParamValidationError),
    /// The world has been dropped, so we should just return.
    #[error("World no longer exists")]
    WorldDropped,
}

/// Future representing a single in-flight ECS access request.
struct AsyncSystemHandleFut<P: SystemParam + 'static, Func, Out> {
    /// Interned system-set key identifying which sync-point queue this future
    /// should be sent to.
    sync_point_key: InternedSystemSet,
    /// This is the pseudo-system that we try to run when we have access to `World`.
    world_fn: WorldFn<Func, P, Out>,
    /// Poll guard for the currently queued wake cycle, if any.
    ///
    /// The future drops this at the end of `poll` which acts as acknowledgement that the `poll`
    /// was called at least once.
    maybe_poll_guard: Option<LatchGuard>,
    /// Weak bridge pointer so the loss of the world becomes a clean runtime error.
    bridge: Weak<BridgeState>,
}

// bundles Func, SystemParam, and Out all together
struct WorldFn<Func, P, Out> {
    _p: PhantomData<(P, Out)>,
    /// This is an option just so we can take it out when we run it so we can use `FnOnce`
    /// instead of `FnMut`, so it's more flexible than real systems.
    func: Option<Func>,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl<Func, P, Out> WorldFn<Func, P, Out>
where
    P: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
{
    // this function attempts to acquire the SystemState lock and execute the inner function:
    // if it can't acquire the lock, it returns None
    // if it can acquire the lock but the inner system_state can't validate, it returns Some(Err)
    // if it can acquire the lock, it calls the inner Func and returns Out. `try_call` should never be called again
    fn try_call(&mut self, world: &mut World) -> Option<Result<Out, AsyncAccessError>> {
        let system_state = self.system_state.clone();
        // Attempt to acquire the typed `SystemState<P>`.
        //
        // We deliberately use `try_lock` rather than blocking. If
        // another bridge request is currently using the same system
        // state, we simply yield and let the sync-point driver try again
        // on a later internal tick.
        let Some(mut system_state) = system_state.try_lock::<P>() else {
            return None;
        };
        if !system_state.meta().is_send() {
            return Some(Err(AsyncAccessError::InvalidParam(
                bevy_ecs::system::SystemParamValidationError::invalid::<
                    bevy_ecs::prelude::NonSend<()>,
                >("Cannot have your system be non-send / exclusive"),
            )));
        }
        let state = match system_state.get_mut(world) {
            Ok(state) => state,
            Err(system_param_validation_error) => {
                return Some(Err(AsyncAccessError::InvalidParam(
                    system_param_validation_error,
                )))
            }
        };
        // We finally have `P::Item<'w, 's>`, yay!, so consume the stored `FnOnce`, run it,
        // and complete the future.
        // This unwrap represents an invariant: try_call can't be called again after it returns Some(Ok).
        Some(Ok(self.func.take().unwrap()(state)))
    }
}

impl<P: SystemParam + 'static, Func, Out> Unpin for AsyncSystemHandleFut<P, Func, Out> {}

impl<P, Func, Out> Future for AsyncSystemHandleFut<P, Func, Out>
where
    P: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
{
    type Output = Result<Out, AsyncAccessError>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        use core::task::Poll;

        // If we were previously woken by the sync-point driver, we will have a
        // `LatchGuard` stored here.
        //
        // Dropping that guard at the end of this poll acts as the
        // acknowledgement that yes, this wake was observed and this task has
        // attempted its run, you may release the waiting on the other side.
        let _maybe_poll_guard = self.maybe_poll_guard.take();

        // Try to gain a strong reference to the bridge. If this fails, the world is gone,
        // so further access is impossible.
        let bridge = match self.bridge.upgrade() {
            None => {
                return Poll::Ready(Err(AsyncAccessError::WorldDropped));
            }
            Some(bridge) => bridge,
        };
        match bridge
            .scoped_world
            .try_with(|world| self.world_fn.try_call(world))
            .ok()
        {
            Some(maybe_out) => match maybe_out {
                // We're done!
                Some(out) => Poll::Ready(out),
                // Couldn't lock SystemState, yield and retry
                None => Poll::Pending,
            },
            None => {
                // No world is currently exposed. That means we are being polled
                // outside the sync-point drive, so we cannot access ECS yet.
                //
                // Instead, enqueue ourselves to be revisited when the matching
                // sync-point system runs.
                let (latch, guard) = LatchGuard::new_pair();
                // Store the guard so it is dropped at the end of the next poll,
                // unblocking the driver's latch.
                self.maybe_poll_guard.replace(guard);
                // Queue the request under this future's target sync point.
                //
                // The queued payload carries the following!
                // 1. The task's waker, so the sync-point driver can wake it.
                // 2. The poll handshake latch, so the driver can wait until the wake has actually
                // been processed.
                // 3. The erased `SystemState` storage itself.
                // A failed queue attempt indicates that the world has been dropped.
                match bridge.request_queues.try_send(
                    self.sync_point_key,
                    PendingRequest {
                        waker: cx.waker().clone(),
                        latch,
                        system_state: self.world_fn.system_state.clone(),
                    },
                ) {
                    Ok(_) => Poll::Pending,
                    Err(_) => Poll::Ready(Err(AsyncAccessError::WorldDropped)),
                }
            }
        }
    }
}
