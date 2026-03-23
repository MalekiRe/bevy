//! Async <-> ECS bridge for Bevy.
//!
//! This crate coordinates three participants that want to share [`&mut World`](bevy_ecs::world::World) access:
//! * The main Bevy schedule
//! * Futures and async tasks running on other threads
//! * The bridge driver between these two (exposed as [`AsyncBridge`] and [`tick_async_bridge`])
//!
//! # Crate-level Invariants
//!
//! * Normal Rust safety invariants for `&mut World` (aliasing)
//! * At most one future has world access at a time
//! * Futures only access the world while the scoped pointer (managed by the bridge driver) is live
//! * [`SystemState`](bevy_ecs::system::SystemState) is always initialized before use
//! * Deferred ops are only applied after every future finishes polling and releases world access
//! * The driver can't deadlock
//! * All futures that want world access can eventually complete (assuming fair scheduling by the
//!   async runtime)
//! * If the world is dropped, futures don't leak and eventually finish (in an error state)
//!
//! # Protocol
//!
//! ```text
//! Futures (tasks on worker threads)
//!     | enqueue requests (creates guarded latch pair)
//!     v
//! Driver (exclusive system, world-owning thread)
//!     1. Drain request queue for this sync point
//!     2. Initialize SystemStates
//!     3. Publish World pointer (via scoped_static_storage). Future access scope begins
//!     4. Wake all drained futures
//!        -> Futures race for locks (non-blocking)
//!        -> Success: acquire both locks, do work, complete
//!        -> Failure: re-enqueue, then signal driver (Drop latch guard)
//!        -> Direct access: non-queued future polled during scope,
//!           bypasses queue, acquires locks, completes (no signal)
//!     5. Wait for all drained future latch guards to drop
//!     6. Unpublish pointer, scope ends.
//!     7. Apply any deferred ops from SystemState of polled futures
//!     8. Loop (up to AsyncTickBudget) or return
//!     v
//! Schedule resumes (normal systems run)
//! ```
//!
//! # Dual locking
//!
//! * The published `World` pointer lock is managed by the
//!   [`ScopedStatic`](scoped_static_storage::ScopedStatic) primitive in
//!   `scoped_static_storage` (only one future can lock this at a time)
//! * [`SystemState`](bevy_ecs::system::SystemState) locks are managed by the `SystemStateCell`
//!   primitive of this crate (futures can share a `SystemState`, but not at the same time)
//!
//! # Preventing driver deadlocks when futures panic
//!
//! If a future panics while holding locks, Rust's panic unwinding drops destructors in reverse
//! scope order:
//! 1. The `SystemState` `MutexGuard` drops (releasing the lock)
//! 2. The `World` pointer's scope `MutexGuard` drops (releasing the lock)
//! 3. The latch guard for this future's `poll()` drops, and the driver is signaled
//!
//! # How futures can fail cleanly
//!
//! * If the async bridge cannot be reached ([`Weak::upgrade()`](bevy_platform::sync::Weak::upgrade)
//!   fails during `poll()`), the world has been dropped and the future cannot complete
//! * If `SystemState`s are invalid, they can't be used and the future cannot complete
//! * Regardless, the future returns `Ready(Err)` and completes permanently

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://bevy.org/assets/icon.png",
    html_favicon_url = "https://bevy.org/assets/icon.png"
)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod access;
mod bridge;
mod guarded_latch;
mod plugin;
mod request;
mod system_state_cell;

pub use crate::access::{AsyncAccessError, AsyncSystemHandle};
pub use crate::bridge::{tick_async_bridge, AsyncBridge};
pub use crate::plugin::AsyncPlugin;

/// The async prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        tick_async_bridge, AsyncAccessError, AsyncBridge, AsyncPlugin, AsyncSystemHandle,
    };
}
