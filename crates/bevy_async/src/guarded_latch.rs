use bevy_platform::sync::Arc;

#[cfg(feature = "std")]
mod inner {
    use bevy_platform::sync::Mutex;
    use std::sync::Condvar;

    pub(super) struct Inner {
        lock: Mutex<bool>,
        cv: Condvar,
    }

    #[inline]
    pub(super) fn new() -> Inner {
        let lock = Mutex::new(false);
        let cv = Condvar::new();
        Inner { lock, cv }
    }

    #[inline]
    pub(super) fn signal(inner: &Inner) {
        let Inner { lock, cv } = inner;
        let mut signaled = lock.lock().unwrap();
        *signaled = true;
        cv.notify_one();
    }

    #[inline]
    pub(super) fn wait(inner: &Inner) {
        let Inner { lock, cv } = inner;
        let mut signaled = lock.lock().unwrap();
        while !*signaled {
            signaled = cv.wait(signaled).unwrap();
        }
    }
}

#[cfg(not(feature = "std"))]
mod inner {
    use bevy_platform::sync::Mutex;

    pub(super) type Inner = Mutex<bool>;

    #[inline]
    pub(super) fn new() -> Inner {
        Mutex::new(false)
    }

    #[inline]
    pub(super) fn signal(inner: &Inner) {
        *inner.lock().unwrap() = true;
    }

    #[inline]
    pub(super) fn wait(inner: &Inner) {
        loop {
            if *inner.lock().unwrap() {
                break;
            }
        }
    }
}

/// Unblocks the paired [`LatchWaiter`] when dropped.
///
/// Signaling on drop means it is guaranteed to fire even if the holder's scope
/// panics or replaces the guard with a new one.
pub(crate) struct LatchGuard(Arc<inner::Inner>);

impl LatchGuard {
    /// Creates a paired [`LatchWaiter`] and [`LatchGuard`].
    #[inline]
    pub(crate) fn new_pair() -> (LatchWaiter, Self) {
        let inner = Arc::new(inner::new());
        (LatchWaiter(inner.clone()), Self(inner))
    }
}

impl Drop for LatchGuard {
    #[inline]
    fn drop(&mut self) {
        inner::signal(&self.0);
    }
}

/// Waits (blocks) until the paired [`LatchGuard`] is dropped.
pub(crate) struct LatchWaiter(Arc<inner::Inner>);

impl LatchWaiter {
    /// Blocks until the paired [`LatchGuard`] is dropped.
    #[inline]
    pub(crate) fn wait(&self) {
        inner::wait(&self.0);
    }
}
