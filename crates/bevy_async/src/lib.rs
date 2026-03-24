//! Async <-> ECS bridge for Bevy.
//!
//! This crate lets async tasks interact with Bevy ECS state safely. It
//! coordinates two participants that want to share
//! [`&mut World`](bevy_ecs::world::World) access:
//!
//! * The main Bevy schedule (owns the `World`)
//! * Futures and async tasks running on worker threads (or the microtask
//!   queue on wasm)
//!
//! # Two access modes
//!
//! | Method | Bounds on closure | Platform | Mechanism |
//! |--------|-------------------|----------|-----------|
//! | [`AsyncParams::run`] | `'static + ConditionalSend` | All (including wasm) | Job queue: driver runs the closure directly |
//! | [`AsyncParams::run_scoped`] | *(none)* | `std` only | Scoped world: future runs the closure during poll |
//!
//! Use [`run`](AsyncParams::run) when you can. Prefer
//! [`run_scoped`](AsyncParams::run_scoped) when you need the closure to
//! borrow from the enclosing async scope (no `'static` requirement).
//!
//! # Crate-level invariants
//!
//! * Normal Rust aliasing rules for `&mut World`
//! * At most one closure runs at a time per sync point
//! * [`SystemState`](bevy_ecs::system::SystemState) is always initialized before use
//! * Deferred ops are applied after each batch of closures completes
//! * Futures that want world access can eventually complete (assuming fair
//!   scheduling by the async runtime)
//! * If the world is dropped, futures resolve with
//!   [`AsyncAccessError::WorldDropped`]
//!
//! # Protocol
//!
//! ## Job queue (`run`), all platforms
//!
//! ```text
//! Async task                         Driver (world-owning thread)
//! ----------                         ----------------------------
//! enqueue Arc<JobCell> ------>       1. Drain job queue
//!                                    2. Skip cancelled jobs
//!                                    3. Initialize SystemStates
//!                                    4. Run each closure with &mut World
//!                                       -> Success: store result
//!                                       -> Lock contended: re-queue
//!                                    5. Apply deferred ops
//!                                    6. Wake futures
//! receive result     <------         7. Loop (up to tick budget) or return
//! ```
//!
//! ## Scoped world (`run_scoped`), `std` only
//!
//! ```text
//! Async task                         Driver (world-owning thread)
//! ----------                         ----------------------------
//! enqueue ScopedRequest ----->       1. Drain scoped request queue
//!                                    2. Initialize SystemStates
//!                                    3. Scope &mut World into shared slot
//!                                    4. Wake futures
//! poll: run closure with  <--        5. Block on latches (wait for polls)
//!       scoped &mut World
//! drop LatchGuard ---------->        6. Un-scope world
//!                                    7. Apply deferred ops
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

mod bridge;
#[cfg(feature = "std")]
mod guarded_latch;
mod job;
mod plugin;
#[cfg(feature = "std")]
mod scoped;
mod system_state_cell;

pub use crate::bridge::{tick_async_bridge, AsyncBridge};
pub use crate::plugin::AsyncPlugin;
pub use bridge::AsyncParams;

/// The async prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::bridge::AsyncParams;
    #[doc(hidden)]
    pub use crate::{tick_async_bridge, AsyncAccessError, AsyncBridge, AsyncPlugin};
}

/// All the different reasons that accessing the ECS asynchronously can fail.
#[derive(thiserror::Error, Debug)]
pub enum AsyncAccessError {
    /// The requested `SystemParam` was invalid in the current world context.
    /// for example trying to access a param that fails Bevy's usual validation like a missing
    /// Resource or using `Single` on something that has 0 or multiple instances.
    #[error(transparent)]
    InvalidParam(bevy_ecs::system::SystemParamValidationError),
    /// The world has been dropped, so we can't ever access it again.
    #[error("World no longer exists")]
    WorldDropped,
    /// The closure panicked during execution. The panic was caught so the
    /// driver could continue processing remaining jobs.
    #[cfg(all(feature = "std", panic = "unwind"))]
    #[error("Closure panicked")]
    Panicked,
}
