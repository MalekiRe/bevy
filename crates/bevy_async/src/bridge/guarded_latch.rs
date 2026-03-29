//! One-shot latch for synchronizing the scoped-world handshake.
//!
//! The future creates both sides via [`LatchGuard::new_pair`], keeps the
//! [`LatchGuard`], and sends the [`LatchWaiter`] to the driver. When the
//! future's poll finishes (or panics), the guard is dropped, signaling
//! the driver that it is safe to un-scope the world.

use bevy_platform::sync::Arc;
use std::sync::{Condvar, Mutex};

/// Unblocks the paired [`LatchWaiter`] when dropped.
///
/// Signaling on drop means it is guaranteed to fire even if the holder's scope
/// panics or replaces the guard with a new one.
pub(crate) struct LatchGuard(Arc<LatchInner>);

/// Waits (blocks) until the paired [`LatchGuard`] is dropped.
pub(crate) struct LatchWaiter(Arc<LatchInner>);

struct LatchInner {
    signaled: Mutex<bool>,
    cv: Condvar,
}

impl LatchGuard {
    /// Creates a paired [`LatchWaiter`] and [`LatchGuard`] for one-shot use.
    pub(crate) fn new_pair() -> (LatchWaiter, Self) {
        let inner = Arc::new(LatchInner {
            signaled: Mutex::new(false),
            cv: Condvar::new(),
        });
        (LatchWaiter(inner.clone()), Self(inner))
    }
}

impl Drop for LatchGuard {
    fn drop(&mut self) {
        *self.0.signaled.lock().unwrap() = true;
        self.0.cv.notify_one();
    }
}

impl LatchWaiter {
    /// Blocks until the paired [`LatchGuard`] is dropped.
    pub(crate) fn wait(&self) {
        let mut signaled = self.0.signaled.lock().unwrap();
        while !*signaled {
            signaled = self.0.cv.wait(signaled).unwrap();
        }
    }
}
