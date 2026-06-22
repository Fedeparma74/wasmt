//! A tiny non-parking spinlock.
//!
//! This is the cornerstone of every primitive in [`crate::sync`]. The
//! reason it exists at all is subtle but critical: on
//! `wasm32-unknown-unknown` a contended `std::sync::Mutex` blocks via
//! `memory.atomic.wait32`, and **`Atomics.wait` is illegal on the
//! browser main thread** — it throws `RuntimeError: Atomics.wait cannot
//! be called in this context`, trapping the wasm instance.
//!
//! Every off-the-shelf async primitive (`tokio::sync::*`,
//! `futures::lock::Mutex`, anything built on `event-listener` /
//! `async-lock`) guards its internal waiter bookkeeping with a
//! `std::sync::Mutex`, so any of them used from the main thread under
//! contention crash. A spinlock never parks the OS thread — it loops on
//! an atomic — so it is safe to take from the main thread. The critical
//! sections it protects here are all O(few instructions) (pushing /
//! popping a waiter node), and a contending main thread only ever spins
//! against a *worker* thread that is guaranteed to be making forward
//! progress in parallel, so the spin is bounded and short.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

/// A non-parking mutual-exclusion lock. Unlike `std::sync::Mutex` it
/// never calls `Atomics.wait`, so it is safe to lock from the browser
/// main thread.
pub(crate) struct Spin<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is gated by the `locked` flag; only one
// thread holds the guard at a time, so `T: Send` is sufficient for both.
unsafe impl<T: Send> Send for Spin<T> {}
unsafe impl<T: Send> Sync for Spin<T> {}

impl<T> Spin<T> {
    pub(crate) const fn new(value: T) -> Self {
        Spin {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub(crate) fn lock(&self) -> SpinGuard<'_, T> {
        // Test-and-test-and-set: the inner relaxed spin avoids hammering
        // the cache line with RMW ops while the lock is held.
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

pub(crate) struct SpinGuard<'a, T> {
    lock: &'a Spin<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
