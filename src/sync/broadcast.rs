//! `broadcast` — a main-thread-safe multi-producer, multi-consumer
//! broadcast channel, API-compatible with `tokio::sync::broadcast`.
//!
//! A bounded ring buffer holds the most recent `cap` values; every
//! receiver reads the full stream from its own cursor. A receiver that
//! falls more than `cap` behind observes [`RecvError::Lagged`] and is
//! fast-forwarded. Values are stored behind `Arc` so each delivery to N
//! receivers clones a pointer, not the payload (`T: Clone`).
//!
//! Internals use the non-parking [`Spin`] lock + a lock-free
//! [`AtomicWaker`]-backed [`Notify`], so it never calls `Atomics.wait`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::notify::Notify;
use super::spin::Spin;

/// Error types, grouped to match `tokio::sync::broadcast::error`.
pub mod error {
    pub use super::{RecvError, SendError, TryRecvError};
}

/// Error returned by [`Receiver::recv`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RecvError {
    /// The channel is closed and drained.
    Closed,
    /// The receiver lagged; `n` messages were skipped.
    Lagged(u64),
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Closed => f.write_str("channel closed"),
            RecvError::Lagged(n) => write!(f, "channel lagged by {n}"),
        }
    }
}
impl std::error::Error for RecvError {}

/// Error returned by [`Receiver::try_recv`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TryRecvError {
    Empty,
    Closed,
    Lagged(u64),
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryRecvError::Empty => f.write_str("channel empty"),
            TryRecvError::Closed => f.write_str("channel closed"),
            TryRecvError::Lagged(n) => write!(f, "channel lagged by {n}"),
        }
    }
}
impl std::error::Error for TryRecvError {}

/// Error returned by [`Sender::send`] when there are no receivers.
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

struct Slot<T> {
    seq: u64,
    val: Arc<T>,
}

struct Inner<T> {
    /// Ring of the most recent `cap` slots, ascending `seq`.
    buffer: VecDeque<Slot<T>>,
    cap: usize,
    /// Sequence number to assign to the next sent value.
    next_seq: u64,
    senders: usize,
    receivers: usize,
}

struct Shared<T> {
    inner: Spin<Inner<T>>,
    notify: Notify,
    closed: AtomicBool,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

/// Sending half. Cloneable; mirrors `tokio::sync::broadcast::Sender`.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// Receiving half. Mirrors `tokio::sync::broadcast::Receiver`. Each
/// receiver reads the full stream from its own cursor.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    /// Next sequence number this receiver expects.
    next: u64,
}

/// Create a broadcast channel holding up to `cap` buffered messages.
pub fn channel<T: Clone>(cap: usize) -> (Sender<T>, Receiver<T>) {
    assert!(cap > 0, "broadcast channel requires capacity > 0");
    let shared = Arc::new(Shared {
        inner: Spin::new(Inner {
            buffer: VecDeque::with_capacity(cap),
            cap,
            next_seq: 0,
            senders: 1,
            receivers: 1,
        }),
        notify: Notify::new(),
        closed: AtomicBool::new(false),
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared, next: 0 },
    )
}

impl<T: Clone> Sender<T> {
    /// Broadcast `value` to all current receivers. Returns the number
    /// of receivers, or `Err` if there are none.
    pub fn send(&self, value: T) -> Result<usize, SendError<T>> {
        let mut g = self.shared.inner.lock();
        if g.receivers == 0 {
            return Err(SendError(value));
        }
        let seq = g.next_seq;
        g.next_seq += 1;
        let slot = Slot {
            seq,
            val: Arc::new(value),
        };
        if g.buffer.len() == g.cap {
            g.buffer.pop_front();
        }
        g.buffer.push_back(slot);
        let n = g.receivers;
        drop(g);
        self.shared.notify.notify_waiters();
        Ok(n)
    }

    /// Create a new receiver starting at the next sent message.
    pub fn subscribe(&self) -> Receiver<T> {
        let next = {
            let mut g = self.shared.inner.lock();
            g.receivers += 1;
            g.next_seq
        };
        self.shared.receiver_count.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            next,
        }
    }

    pub fn receiver_count(&self) -> usize {
        self.shared.receiver_count.load(Ordering::Acquire)
    }

    /// Number of messages currently buffered.
    pub fn len(&self) -> usize {
        self.shared.inner.lock().buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shared.inner.lock().buffer.is_empty()
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.inner.lock().senders += 1;
        self.shared.sender_count.fetch_add(1, Ordering::AcqRel);
        Sender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let last = {
            let mut g = self.shared.inner.lock();
            g.senders -= 1;
            g.senders == 0
        };
        self.shared.sender_count.fetch_sub(1, Ordering::AcqRel);
        if last {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.notify.notify_waiters();
        }
    }
}

impl<T: Clone> Receiver<T> {
    /// Receive the next message for this receiver, waiting if needed.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Lagged(n)) => return Err(RecvError::Lagged(n)),
                Err(TryRecvError::Closed) => return Err(RecvError::Closed),
                Err(TryRecvError::Empty) => {
                    let notified = self.shared.notify.notified();
                    // Re-check after arming so we don't miss a send /
                    // close that raced the registration.
                    match self.peek_state() {
                        PeekState::Ready | PeekState::Closed => continue,
                        PeekState::Empty => notified.await,
                    }
                }
            }
        }
    }

    /// Try to receive without waiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let g = self.shared.inner.lock();
        let oldest = g.buffer.front().map(|s| s.seq);
        if let Some(oldest) = oldest {
            if self.next < oldest {
                // We lagged: skip ahead to the oldest retained message.
                let skipped = oldest - self.next;
                self.next = oldest;
                return Err(TryRecvError::Lagged(skipped));
            }
            if self.next < g.next_seq {
                // Message available at index (next - oldest).
                let idx = (self.next - oldest) as usize;
                let val = g.buffer[idx].val.clone();
                self.next += 1;
                return Ok((*val).clone());
            }
        }
        // Nothing buffered at/after our cursor.
        if self.shared.closed.load(Ordering::Acquire) && self.next >= g.next_seq {
            return Err(TryRecvError::Closed);
        }
        Err(TryRecvError::Empty)
    }

    fn peek_state(&self) -> PeekState {
        let g = self.shared.inner.lock();
        if let Some(front) = g.buffer.front()
            && (self.next < front.seq || self.next < g.next_seq)
        {
            return PeekState::Ready;
        }
        if self.shared.closed.load(Ordering::Acquire) && self.next >= g.next_seq {
            return PeekState::Closed;
        }
        PeekState::Empty
    }

    /// A fresh receiver starting at the next message (does not inherit
    /// this receiver's backlog).
    pub fn resubscribe(&self) -> Receiver<T> {
        let next = {
            let mut g = self.shared.inner.lock();
            g.receivers += 1;
            g.next_seq
        };
        self.shared.receiver_count.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            next,
        }
    }

    pub fn len(&self) -> usize {
        let g = self.shared.inner.lock();
        g.next_seq.saturating_sub(self.next) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

enum PeekState {
    Ready,
    Empty,
    Closed,
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.inner.lock().receivers += 1;
        self.shared.receiver_count.fetch_add(1, Ordering::AcqRel);
        Receiver {
            shared: self.shared.clone(),
            next: self.next,
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.inner.lock().receivers -= 1;
        self.shared.receiver_count.fetch_sub(1, Ordering::AcqRel);
    }
}
