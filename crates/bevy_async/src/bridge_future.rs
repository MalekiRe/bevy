use crate::bridge_request;
use crate::bridge_request::BridgeRequest;
use crate::plugin::AsyncWorld;
use crate::system_state::{ErasedSystemStateCell, SystemStateCell};
use alloc::{borrow::ToOwned, boxed::Box, vec::Vec};
use bevy_ecs::system::SystemParamValidationError;
use bevy_ecs::{
    schedule::{InternedSystemSet, IntoSystemSet, SystemSet},
    system::{SystemParam, SystemParamItem},
    world::World,
};
use bevy_platform::sync::{Arc, Mutex, PoisonError};
use core::any::Any;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use variadics_please::all_tuples;

/// A `FnOnce` that can be run as a bridge system in order to allow for our bridge function to
/// get type inference off the closure.
pub trait AsyncSystemParamFunction<Marker>: Send {
    type Out: Send + 'static;
    type Param: SystemParam + 'static;

    /// Converts this function into an Arc with a static lifetime.
    fn to_runnable(self, system_state: Arc<dyn ErasedSystemStateCell>) -> RunnableBridgeFunction;
}

impl<Out, Func, F0: SystemParam + 'static> AsyncSystemParamFunction<fn(F0) -> Out> for Func
where
    Func: FnOnce(F0) -> Out + FnOnce(SystemParamItem<F0>) -> Out + Send,
    Out: Send + 'static,
{
    type Out = Out;
    type Param = F0;

    fn to_runnable(self, system_state: Arc<dyn ErasedSystemStateCell>) -> RunnableBridgeFunction {
        RunnableBridgeFunction::from_func(move |world: &mut World| {
            // Lock the system state. The unwrap is safe since we only try_lock when we have
            // exclusive world access, so the lock must not be contested.
            let mut system_state = system_state
                .try_lock::<F0>(world)
                .expect("Lock should never be contended since we have exclusive world access");

            let param = system_state.get_mut(world)?;
            let out = (self)(param);
            system_state.apply(world);

            Ok(out)
        })
    }
}

macro_rules! impl_system_param_function {
    ($($F:ident),*) => {
        #[expect(
            clippy::allow_attributes,
            reason = "This is a tuple-related macro; as such, the lints below may not always apply."
        )]
        #[allow(
            non_snake_case,
            reason = "The names of these variables are provided by the caller, not by us."
        )]
        impl<Out, Func, $($F: SystemParam + 'static),*> AsyncSystemParamFunction<fn($($F),*) -> Out> for Func
        where
            Func: FnOnce($($F),*) -> Out + FnOnce($(SystemParamItem<$F>),*) -> Out + Send,
            Out: Send + 'static,
        {
            type Out = Out;
            type Param = ($($F,)*);

            fn to_runnable(self, system_state: Arc<dyn ErasedSystemStateCell>) -> RunnableBridgeFunction {
                RunnableBridgeFunction::from_func(move |world: &mut World| {
                    // Lock the system state. The unwrap is safe since we only try_lock when we have
                    // exclusive world access, so the lock must not be contested.
                    let mut system_state = system_state
                        .try_lock::<($($F,)*)>(world)
                        .expect("Lock should never be contended since we have exclusive world access");

                    let ($($F,)*) = system_state.get_mut(world)?;

                    fn call_inner<Out, $($F),*>(f: impl FnOnce($($F),*) -> Out, $($F: $F),*) -> Out {
                        f($($F),*)
                    }
                    let out = call_inner(self, $($F),*);
                    system_state.apply(world);

                    Ok(out)
                })
            }
        }
    };
}

all_tuples!(impl_system_param_function, 2, 16, F);

/// Handle that lets an async task request temporary access to an ECS
/// `SystemParam` or a tuple of them.
///
/// `P` is the typed system parameter the caller eventually wants, such as:
/// - [`bevy_ecs::prelude::Commands`]
/// - [`bevy_ecs::prelude::Res`]
/// - [`bevy_ecs::prelude::Query`]
///
/// It is cheap to clone and intended to be passed into async tasks.
/// You can pass it into *multiple* tasks on separate threads and have them work concurrently
/// off of the same state, sharing `Locals`.
pub struct AsyncSystemState<P: SystemParam + 'static> {
    pub(crate) _p: PhantomData<P>,

    /// A `Weak` is used so tasks do not stay alive if the world is dropped.
    /// If the world goes away, upgrading this weak pointer fails and access
    /// returns [`BridgeError::WorldDropped`].
    pub(crate) world: AsyncWorld,

    /// Type-erased storage for the underlying `SystemState<P>`.
    ///
    /// Each `EcsAccess<P>` keeps reusing the same typed system state across
    /// accesses so repeated operations do not rebuild it from scratch.
    ///
    /// This is also important not only to persist params like `Local` but *also* so `Changed` and
    /// `Added` and other filters can work.
    pub(crate) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl<P: SystemParam + 'static> Clone for AsyncSystemState<P> {
    fn clone(&self) -> Self {
        Self {
            _p: PhantomData,
            world: self.world.clone(),
            system_state: self.system_state.clone(),
        }
    }
}

impl<P: SystemParam + 'static> AsyncSystemState<P> {
    /// Create a new system state from an [`AsyncWorld`] matching the API surface of [`SystemState`]
    /// with [`World`].
    ///
    /// [`SystemState`]: bevy_ecs::system::SystemState
    /// [`World`]: bevy_ecs::world::World
    pub(crate) fn new(world: AsyncWorld) -> Self {
        Self {
            _p: PhantomData,
            world,
            #[cfg(feature = "std")]
            system_state: Arc::new(SystemStateCell::<P>::default()),
            #[cfg(not(feature = "std"))]
            system_state: Arc::from(
                Box::new(SystemStateCell::<P>::default()) as Box<dyn ErasedSystemStateCell>
            ),
        }
    }

    /// This function allows us to create a bridge between the async task we are in and the ecs
    /// world we want access to, effectively running a system from an async task. The systems run
    /// here are able to take in `&` and `&mut` variables from the surrounding context unlike
    /// standard Bevy systems.
    ///
    /// We bridge *at* the `_sync_point` `SyncPoint` with our `bridge_fn`.
    ///
    pub fn bridge<Marker, BridgeFn, SyncPoint: 'static>(
        &self,
        _sync_point: SyncPoint,
        bridge_fn: BridgeFn,
    ) -> BridgeFuture<BridgeFn, Marker>
    where
        Marker: 'static,
        BridgeFn: AsyncSystemParamFunction<Marker, Param = P>,
    {
        // This function returns the concrete [`BridgeFuture`] rather than being an `async fn` so that the
        // future's `Send`-ness is structural, which keeps multi-parameter closures usable inside
        // `Send` tasks (an `async fn`'s opaque future trips rust's higher-ranked lifetime checks
        // there).
        BridgeFuture {
            system_set: bridge_request::async_world_sync_point::<SyncPoint>
                .into_system_set()
                .intern(),
            bridge_fn_state: Arc::new(Mutex::new(BridgeFunctionState::Runnable(
                bridge_fn.to_runnable(self.system_state.clone()),
            ))),
            requested: false,
            world: self.world.clone(),
            _marker_1: PhantomData,
            _marker_2: PhantomData,
        }
    }
}

/// If the bridge cannot run, either because the system params were invalid, or because the world it
/// was referencing no longer exists, we return this error.
#[derive(thiserror::Error, Debug)]
pub enum BridgeError {
    /// The requested `SystemParam` was invalid in the current world context.
    /// for example trying to access a param that fails Bevy's usual validation like a missing
    /// Resource or using `Single` on something that has 0 or multiple instances.
    #[error(transparent)]
    SystemParamValidation(#[from] SystemParamValidationError),
    /// The world has been dropped, so we should just return.
    #[error("World no longer exists")]
    WorldDropped,
}

/// Future representing a single in-flight bridging request between our async task and our `World`.
pub struct BridgeFuture<Func, Marker> {
    /// Interned system-set key identifying which sync-point queue this future
    /// should be sent to.
    system_set: InternedSystemSet,
    /// This is the pseudo-system that we try to run when we have access to `World` with its current
    /// state.
    bridge_fn_state: Arc<Mutex<BridgeFunctionState>>,
    /// Whether this future has requested to run.
    requested: bool,
    /// Weak bridge pointer so the loss of the world becomes a clean runtime error.
    world: AsyncWorld,
    _marker_1: PhantomData<fn() -> Marker>,
    // Unlike above, we want the future to know that it internally holds a `Func`, so don't use the
    // "covariant generic" PhantomData above.
    _marker_2: PhantomData<Func>,
}

impl<Func, Marker> Drop for BridgeFuture<Func, Marker> {
    fn drop(&mut self) {
        // `bridge_fn_state` might have references to the outer scope (in either the bridge function
        // or its return value). So we must make sure to drop the bridge function or its return
        // value before this future is cancelled/dropped, despite the fact that the Arc could keep
        // it alive.
        *self
            .bridge_fn_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = BridgeFunctionState::Terminated;
    }
}

impl<Func, Marker> Unpin for BridgeFuture<Func, Marker> {}

impl<Func, Marker> Future for BridgeFuture<Func, Marker>
where
    Marker: 'static,
    Func: AsyncSystemParamFunction<Marker>,
{
    type Output = Result<Func::Out, BridgeError>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        use core::task::Poll;
        // Unpin, because we don't care about pinning.
        let this = self.get_mut();

        // Try to gain a strong reference to the bridge. If this fails, the world is gone,
        // so further access is impossible.
        let Some(strong_world) = this.world.0.upgrade() else {
            return Poll::Ready(Err(BridgeError::WorldDropped));
        };

        let mut bridge_fn = this
            .bridge_fn_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Make sure no one is polling this future after it's complete.
        match &*bridge_fn {
            BridgeFunctionState::Runnable(_) | BridgeFunctionState::Finished(_) => {}
            // We only ever set Terminated if we've polled a finished function, or if we've
            // cancelled the future (which clearly hasn't happened since we haven't dropped this
            // future). Panic here to make sure no one is polling this future after it's complete.
            BridgeFunctionState::Terminated => {
                panic!("polling a BridgeFuture that has already completed")
            }
        }

        if !this.requested {
            this.requested = true;
            strong_world
                .bridge_requests
                .try_send(
                    &this.system_set,
                    BridgeRequest {
                        bridge_fn: this.bridge_fn_state.clone(),
                        waker: cx.waker().clone(),
                    },
                )
                .ok()
                .unwrap();
            Poll::Pending
        } else {
            // Hokey-pokey the bridge function.
            match core::mem::replace(&mut *bridge_fn, BridgeFunctionState::Terminated) {
                f @ BridgeFunctionState::Runnable(_) => {
                    // Put the function back if it's still runnable.
                    *bridge_fn = f;
                    Poll::Pending
                }
                BridgeFunctionState::Finished(value) => {
                    let value = value?;
                    // Unwrap is safe because BridgeFunctionState guarantees that it always holds
                    // the return value of the function.
                    let value = *value.downcast::<Func::Out>().unwrap();
                    Poll::Ready(Ok(value))
                }
                // Handled above.
                BridgeFunctionState::Terminated => unreachable!(),
            }
        }
    }
}

/// A function that can be run for async bridge functions.
pub struct RunnableBridgeFunction(
    Box<dyn FnOnce(&mut World) -> Result<Box<dyn Any + Send>, SystemParamValidationError> + Send>,
);

impl RunnableBridgeFunction {
    /// Creates an instance storing `func` that can later be run.
    fn from_func<
        Out: Send + 'static,
        F: FnOnce(&mut World) -> Result<Out, SystemParamValidationError> + Send,
    >(
        func: F,
    ) -> Self {
        let func = SmuggledValue::new(func);
        Self(Box::new(move |world: &mut World| {
            #[expect(
                unsafe_code,
                reason = "we need to be able to smuggle the lifetimes into this closure, otherwise we can't run this future on a different thread"
            )]
            // SAFETY: We stored the original `func` inside the SmuggledValue, which has type `F`.
            let func = unsafe { func.smuggle::<F>() };
            let out = func(world)?;
            Ok(Box::new(out))
        }))
    }

    /// Runs this function on the given world.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the references contained within this function are still valid.
    #[expect(
        unsafe_code,
        reason = "since we've smuggled references into the Box<dyn FnOnce>, we need to be careful in how this function gets called"
    )]
    pub(crate) unsafe fn run(
        self,
        world: &mut World,
    ) -> Result<Box<dyn Any + Send>, SystemParamValidationError> {
        // SAFETY: The caller guarantees that the references within this function are still valid.
        self.0(world)
    }
}

/// The state of the bridge function's execution.
pub(crate) enum BridgeFunctionState {
    /// The function is runnable with the given bridge function.
    Runnable(RunnableBridgeFunction),
    /// The function has already ran, and is now storing its return value.
    ///
    /// This is guaranteed to be the return value of the function from the [`Self::Runnable`] state.
    Finished(Result<Box<dyn Any + Send>, SystemParamValidationError>),
    /// The function is in its terminating state.
    ///
    /// This either means the function ran and its return value was consumed, or execution was
    /// cancelled before the function ran.
    Terminated,
}

impl BridgeFunctionState {
    /// Runs the bridge function (if it is present) and stores the finished value.
    ///
    /// Panics if the state is [`Self::Finished`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the references contained within this function are still valid
    /// (unless the state is [`Self::Terminated`]).
    #[expect(
        unsafe_code,
        reason = "we can't ensure that the references are safe based on only this type - it is a structural relationship in how this type is used"
    )]
    pub(crate) unsafe fn run(&mut self, world: &mut World) {
        match core::mem::replace(self, Self::Terminated) {
            Self::Runnable(function) => {
                // SAFETY: The safety requirements of this function are the same as `run`.
                let out = unsafe { function.run(world) };
                *self = Self::Finished(out);
            }
            // We only run functions once, and always on Runnable functions.
            Self::Finished(_) => unreachable!(),
            Self::Terminated => {}
        }
    }
}

/// A value that has been stored as a raw set of bytes with no type information.
///
/// This is effectively a `Box<dyn Any>`, except without the constraint that the value is 'static.
/// As a consequence, this type does not store any type information, making it the user's job to
/// ensure the type stored and the type extracted are the **exact** same.
pub(crate) struct SmuggledValue {
    /// The data for the value being stored.
    ///
    /// Note: there's no guarantee that this type is correctly aligned, so casting it directly to
    /// the inner type is invalid.
    data: Vec<MaybeUninit<u8>>,
    /// The function to call when dropping this type.
    ///
    /// This allows us to drop the underlying value even after the type information is gone.
    drop_fn: fn(&[MaybeUninit<u8>]),
}

impl SmuggledValue {
    /// Creates a new instance containing `value`.
    fn new<T>(value: T) -> Self {
        #[expect(
            unsafe_code,
            reason = "we need to erase the type, but we still need to store its bytes"
        )]
        // NOTE: Because we're copying through a MaybeUninit, we preserve pointer-provenance (based
        // on https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#validity).
        // SAFETY: We know we are pointing to valid memory (since we are pointing to a valid
        // `value`), and we know there are at least size_of::<T>() bytes in a `T`.
        let value_as_bytes = unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(&value).cast::<MaybeUninit<u8>>(),
                size_of::<T>(),
            )
        }
        .to_owned();

        // Forget the value, Self is now responsible for dropping the value.
        core::mem::forget(value);

        Self {
            data: value_as_bytes,
            drop_fn: |value_as_bytes: &[MaybeUninit<u8>]| {
                // Convert the bytes into a value, and then allow it to be dropped.

                #[expect(
                    unsafe_code,
                    reason = "we've erased the type, so we need to 'recover' the type here"
                )]
                // SAFETY: The bytes make up a valid `T` since we copied those bytes on construction
                // from a valid `T` and the caller guarantees these are the same `T`.
                let _ = unsafe { bytes_to_value::<T>(value_as_bytes) };
            },
        }
    }

    /// Constructs the provided type from the previously stored bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `T` was previously stored in this value.
    #[expect(
        unsafe_code,
        reason = "since we've lost the type information, we have to rely on the user calling this function with the correct type, which is unsafe"
    )]
    unsafe fn smuggle<T>(mut self) -> T {
        // Clear the drop_fn since we no longer own the thing being dropped.
        self.drop_fn = |_: &[MaybeUninit<u8>]| {};

        // SAFETY: The bytes make up a valid `T` since we copied those bytes on construction from a
        // valid `T` and the caller guarantees these are the same `T`. We also know this value
        // hasn't been dropped, since we only drop when self drops (which hasn't happened
        // obviously).
        unsafe { bytes_to_value(self.data.as_ref()) }
    }
}

impl Drop for SmuggledValue {
    fn drop(&mut self) {
        (self.drop_fn)(self.data.as_ref());
    }
}

/// Converts a set of raw bytes into a value of type T.
///
/// # Safety
///
/// The caller must ensure that `bytes` contains bytes that actually makes up a valid T (but does
/// not have to be a valid T, e.g., `bytes` are not aligned). The value must also not have been
/// dropped yet.
#[expect(
    unsafe_code,
    reason = "we don't know whether the bytes are actually a T, so we need unsafe so that users promise that is the case"
)]
unsafe fn bytes_to_value<T>(bytes: &[MaybeUninit<u8>]) -> T {
    debug_assert_eq!(bytes.len(), size_of::<T>());

    let mut return_value: MaybeUninit<T> = MaybeUninit::uninit();
    // SAFETY: We know this is a valid pointer to a slice of bytes, and we know there are at least
    // size_of::<T>() bytes in a `T`.
    let return_value_as_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            return_value.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            size_of::<T>(),
        )
    };

    return_value_as_bytes.copy_from_slice(bytes);

    // SAFETY: The caller ensures that `bytes` holds bytes that actually make a valid `T`, and we
    // copied those bytes into `return_value` (meaning that the bytes are correctly aligned).
    unsafe { return_value.assume_init() }
}
