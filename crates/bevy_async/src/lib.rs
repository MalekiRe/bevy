//!
//! This crate lets async tasks interact with Bevy ECS state safely. It
//! coordinates the main bevy Schedule and the tasks to safely share the [`&mut World`](bevy_ecs::world::World)
//!
//! # Access modes
//!
//! | Method | Bounds on closure | Platform | Mechanism |
//! |--------|-------------------|----------|-----------|
//! | [`AsyncSystemState::bridge`] | *(none)* | `std` only | Scoped world: future runs the closure during poll |
//!
//! (eventually there will be a second mode with `'static+ConditionalSend` closure bounds that works on all platforms)
//!
//! # Crate-level invariants
//!
//! * Normal Rust aliasing rules for [`&mut World`](bevy_ecs::world::World)
//! * At most one closure runs at a time per sync point
//! * [`SystemState`](bevy_ecs::system::SystemState) is always initialized before use
//! * Deferred ops are applied after each batch of closures completes
//! * Futures that want world access can eventually complete (assuming fair scheduling by the async runtime)
//! * If the world is dropped, futures resolve with [`EcsAccessError::WorldDropped`]
//!
//! # Protocol
//!
//! ## "Bridged" closure, native `std` only
//!
//! ```text
//! Async task                         AsyncWorld (world-owned resource on world-owning thread)
//! ----------                         ----------------------------
//! enqueue BridgeRequest ----->        1. Drain bridge request queue
//!       (with drop guard)             2. Scope &mut World into shared slot
//!                                     3. Wake futures
//! poll: initialize system <---        4. Block on drop guards (wait for polls)
//!       state and run closure
//!       with scoped &mut World
//! drop LatchGuard ----------->        5. Un-scope world
//!                                     6. Apply deferred ops
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://bevy.org/assets/icon.png",
    html_favicon_url = "https://bevy.org/assets/icon.png"
)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod plugin;
mod system_state;
mod wake_signal;
mod world;

#[cfg(feature = "std")]
mod bridge;

pub use plugin::AsyncPlugin;
pub use world::{async_world_sync_point, AsyncSystemState, AsyncWorld};

/// The async prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        async_world_sync_point, AsyncPlugin, AsyncSystemState, AsyncWorld, EcsAccessError,
    };
}

/// All the different reasons that accessing the ECS asynchronously can fail.
#[derive(thiserror::Error, Debug)]
pub enum EcsAccessError {
    /// The requested `SystemParam` was invalid in the current world context.
    /// for example trying to access a param that fails Bevy's usual validation like a missing
    /// Resource or using `Single` on something that has 0 or multiple instances.
    #[error(transparent)]
    SystemParamValidation(bevy_ecs::system::SystemParamValidationError),
    /// The world has been dropped, so we can't ever access it again.
    #[error("World no longer exists")]
    WorldDropped,
    /// The closure panicked during execution. The panic was caught so the
    /// schedule could continue.
    #[cfg(all(feature = "std", panic = "unwind"))]
    #[error("Closure panicked")]
    Panicked,
}

/// Internal helper that runs the user closure, catching panics so one bad closure doesn't take down
/// the entire schedule.
#[cfg(all(feature = "std", panic = "unwind"))]
#[inline(always)]
pub(crate) fn invoke<Func, Args, Out>(func: Func, args: Args) -> Result<Out, EcsAccessError>
where
    Func: FnOnce(Args) -> Out,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func(args)))
        .map_err(|_| EcsAccessError::Panicked)
}

/// Fallback when `catch_unwind` is unavailable. The panic propagates normally.
#[cfg(not(all(feature = "std", panic = "unwind")))]
#[inline(always)]
fn invoke<Func, Args, Out>(func: Func, args: Args) -> Result<Out, AsyncAccessError>
where
    Func: FnOnce(Args) -> Out,
{
    Ok(func(args))
}
