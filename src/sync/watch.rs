//! `watch` — a main-thread-safe single-value broadcast channel,
//! API-compatible with `tokio::sync::watch`.
//!
//! The value lives behind a non-parking [`Spin`] lock (so `borrow` and
//! `send` are synchronous and main-safe, matching tokio). As with
//! tokio, a `Ref` from `borrow` must not be held across an `.await`.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::notify::Notify;
use super::spin::{Spin, SpinGuard};

/// Error types, grouped to match `tokio::sync::watch::error`.
pub mod error {
    pub use super::{RecvError, SendError};
}

/// Error returned by [`Sender::send`] when all receivers have dropped.
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendError(..)")
    }
}
impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no receivers")
    }
}
impl<T> std::error::Error for SendError<T> {}

/// Error returned by [`Receiver::changed`] when the sender has dropped.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct RecvError(());

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sender dropped")
    }
}
impl std::error::Error for RecvError {}

struct Shared<T> {
    value: Spin<T>,
    version: AtomicUsize,
    notify: Notify,
    sender_alive: AtomicBool,
    receivers: AtomicUsize,
}

/// Sending half of a watch channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// Receiving half of a watch channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    seen: usize,
}

/// Create a watch channel seeded with `init`.
pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        value: Spin::new(init),
        version: AtomicUsize::new(0),
        notify: Notify::new(),
        sender_alive: AtomicBool::new(true),
        receivers: AtomicUsize::new(1),
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared, seen: 0 },
    )
}

/// A borrow of the watched value. Holds the value lock — do not hold
/// across an `.await`.
pub struct Ref<'a, T> {
    guard: SpinGuard<'a, T>,
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> Sender<T> {
    /// Replace the value and notify all receivers.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.shared.receivers.load(Ordering::Acquire) == 0 {
            return Err(SendError(value));
        }
        *self.shared.value.lock() = value;
        self.shared.version.fetch_add(1, Ordering::Release);
        self.shared.notify.notify_waiters();
        Ok(())
    }

    /// Replace the value, returning the previous one. Always succeeds.
    pub fn send_replace(&self, value: T) -> T {
        let prev = std::mem::replace(&mut *self.shared.value.lock(), value);
        self.shared.version.fetch_add(1, Ordering::Release);
        self.shared.notify.notify_waiters();
        prev
    }

    /// Modify the value in place and notify receivers.
    pub fn send_modify<F: FnOnce(&mut T)>(&self, modify: F) {
        {
            let mut g = self.shared.value.lock();
            modify(&mut g);
        }
        self.shared.version.fetch_add(1, Ordering::Release);
        self.shared.notify.notify_waiters();
    }

    /// Borrow the current value.
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            guard: self.shared.value.lock(),
        }
    }

    /// `true` if no receivers remain.
    pub fn is_closed(&self) -> bool {
        self.shared.receivers.load(Ordering::Acquire) == 0
    }

    /// Number of live receivers.
    pub fn receiver_count(&self) -> usize {
        self.shared.receivers.load(Ordering::Acquire)
    }

    /// Subscribe a new receiver. It will not observe the current value
    /// as "changed" until the next send.
    pub fn subscribe(&self) -> Receiver<T> {
        self.shared.receivers.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            seen: self.shared.version.load(Ordering::Acquire),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.sender_alive.store(false, Ordering::Release);
        self.shared.notify.notify_waiters();
    }
}

impl<T> Receiver<T> {
    /// Borrow the most recent value without marking it seen.
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            guard: self.shared.value.lock(),
        }
    }

    /// Borrow the most recent value and mark it seen (so the next
    /// [`changed`](Self::changed) waits for a newer one).
    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        self.seen = self.shared.version.load(Ordering::Acquire);
        Ref {
            guard: self.shared.value.lock(),
        }
    }

    /// `true` if the value changed since it was last seen.
    pub fn has_changed(&self) -> Result<bool, RecvError> {
        if !self.shared.sender_alive.load(Ordering::Acquire)
            && self.shared.version.load(Ordering::Acquire) == self.seen
        {
            return Err(RecvError(()));
        }
        Ok(self.shared.version.load(Ordering::Acquire) != self.seen)
    }

    /// Wait until the value changes from the last-seen version.
    pub async fn changed(&mut self) -> Result<(), RecvError> {
        loop {
            let cur = self.shared.version.load(Ordering::Acquire);
            if cur != self.seen {
                self.seen = cur;
                return Ok(());
            }
            if !self.shared.sender_alive.load(Ordering::Acquire) {
                // Re-check version once more: a send may have landed
                // just before the sender dropped.
                let cur = self.shared.version.load(Ordering::Acquire);
                if cur != self.seen {
                    self.seen = cur;
                    return Ok(());
                }
                return Err(RecvError(()));
            }
            let notified = self.shared.notify.notified();
            // Re-check after arming the notification (no lost wakeup).
            if self.shared.version.load(Ordering::Acquire) != self.seen
                || !self.shared.sender_alive.load(Ordering::Acquire)
            {
                continue;
            }
            notified.await;
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.receivers.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            seen: self.seen,
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.receivers.fetch_sub(1, Ordering::AcqRel);
    }
}
