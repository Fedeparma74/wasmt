//! Multi-producer, single-consumer channels — main-thread-safe,
//! tokio-compatible. Bounded capacity is enforced with a
//! [`super::Semaphore`]; the message buffer is a lock-free
//! `crossbeam_queue::SegQueue`; the consumer waits on a lock-free
//! [`AtomicWaker`]. None of these call `Atomics.wait`, so the channel
//! is safe on the main thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use crossbeam_queue::SegQueue;
use futures::task::AtomicWaker;

use super::notify::Notify;
use super::semaphore::Semaphore;

pub mod error {
    use std::fmt;

    /// Error returned by `send` when the receiver is gone.
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub struct SendError<T>(pub T);

    impl<T> fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("SendError(..)")
        }
    }
    impl<T> fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("channel closed")
        }
    }
    impl<T> std::error::Error for SendError<T> {}

    /// Error returned by `try_send`.
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub enum TrySendError<T> {
        Full(T),
        Closed(T),
    }

    impl<T> TrySendError<T> {
        pub fn into_inner(self) -> T {
            match self {
                TrySendError::Full(t) | TrySendError::Closed(t) => t,
            }
        }
    }
    impl<T> fmt::Debug for TrySendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TrySendError::Full(_) => f.write_str("Full(..)"),
                TrySendError::Closed(_) => f.write_str("Closed(..)"),
            }
        }
    }
    impl<T> fmt::Display for TrySendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TrySendError::Full(_) => f.write_str("channel full"),
                TrySendError::Closed(_) => f.write_str("channel closed"),
            }
        }
    }
    impl<T> std::error::Error for TrySendError<T> {}

    /// Error returned by `try_recv`.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    pub enum TryRecvError {
        Empty,
        Disconnected,
    }

    impl fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TryRecvError::Empty => f.write_str("channel empty"),
                TryRecvError::Disconnected => f.write_str("channel disconnected"),
            }
        }
    }
    impl std::error::Error for TryRecvError {}
}

pub use error::{SendError, TryRecvError, TrySendError};

struct Chan<T> {
    /// Lock-free MPSC message buffer. `SegQueue` is wait-free in the
    /// common path and never parks, so it is main-thread-safe.
    queue: SegQueue<T>,
    /// Capacity permits for a bounded channel; `None` if unbounded.
    cap: Option<Semaphore>,
    recv_waker: AtomicWaker,
    /// Number of live `Sender`/`UnboundedSender` clones.
    senders: AtomicUsize,
    /// Receiver has been dropped or explicitly closed.
    closed: AtomicBool,
    /// Notifies senders blocked in `closed().await`.
    on_close: Notify,
}

impl<T> Chan<T> {
    fn pop(&self) -> Option<T> {
        let v = self.queue.pop();
        if v.is_some()
            && let Some(sem) = &self.cap
        {
            sem.add_permits(1);
        }
        v
    }
}

// ----- bounded -----

/// Sender half of a bounded channel. Cloneable. Mirrors
/// `tokio::sync::mpsc::Sender`.
pub struct Sender<T> {
    chan: Arc<Chan<T>>,
}

/// Receiver half. Mirrors `tokio::sync::mpsc::Receiver`.
pub struct Receiver<T> {
    chan: Arc<Chan<T>>,
}

/// Create a bounded channel with room for `buffer` messages.
pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0, "mpsc bounded channel requires capacity > 0");
    let chan = Arc::new(Chan {
        queue: SegQueue::new(),
        cap: Some(Semaphore::new(buffer)),
        recv_waker: AtomicWaker::new(),
        senders: AtomicUsize::new(1),
        closed: AtomicBool::new(false),
        on_close: Notify::new(),
    });
    (Sender { chan: chan.clone() }, Receiver { chan })
}

impl<T> Sender<T> {
    /// Send a value, waiting for capacity if the channel is full.
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.chan.closed.load(Ordering::Acquire) {
            return Err(SendError(value));
        }
        let sem = self.chan.cap.as_ref().expect("bounded channel");
        match sem.acquire().await {
            Err(_) => return Err(SendError(value)),
            Ok(p) => p.forget(),
        }
        if self.chan.closed.load(Ordering::Acquire) {
            // Receiver closed between our acquire and now: return the
            // permit and report closure.
            sem.add_permits(1);
            return Err(SendError(value));
        }
        self.chan.queue.push(value);
        self.chan.recv_waker.wake();
        Ok(())
    }

    /// Try to send without waiting.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        if self.chan.closed.load(Ordering::Acquire) {
            return Err(TrySendError::Closed(value));
        }
        let sem = self.chan.cap.as_ref().expect("bounded channel");
        match sem.try_acquire() {
            Ok(p) => {
                p.forget();
                if self.chan.closed.load(Ordering::Acquire) {
                    sem.add_permits(1);
                    return Err(TrySendError::Closed(value));
                }
                self.chan.queue.push(value);
                self.chan.recv_waker.wake();
                Ok(())
            }
            Err(super::semaphore::TryAcquireError::Closed) => Err(TrySendError::Closed(value)),
            Err(super::semaphore::TryAcquireError::NoPermits) => Err(TrySendError::Full(value)),
        }
    }

    /// `true` once the receiver is dropped or closed.
    pub fn is_closed(&self) -> bool {
        self.chan.closed.load(Ordering::Acquire)
    }

    /// Resolves when the receiver is dropped or closed.
    pub async fn closed(&self) {
        loop {
            if self.chan.closed.load(Ordering::Acquire) {
                return;
            }
            let n = self.chan.on_close.notified();
            if self.chan.closed.load(Ordering::Acquire) {
                return;
            }
            n.await;
        }
    }

    /// Available capacity right now.
    pub fn capacity(&self) -> usize {
        self.chan
            .cap
            .as_ref()
            .map(|s| s.available_permits())
            .unwrap_or(usize::MAX)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.senders.fetch_add(1, Ordering::AcqRel);
        Sender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.chan.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Last sender gone — wake the receiver so it observes EOF.
            self.chan.recv_waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// Receive the next message, or `None` once all senders are gone
    /// and the buffer is drained.
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Poll form of [`recv`](Self::recv).
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if let Some(v) = self.chan.pop() {
            return Poll::Ready(Some(v));
        }
        self.chan.recv_waker.register(cx.waker());
        if let Some(v) = self.chan.pop() {
            return Poll::Ready(Some(v));
        }
        if self.chan.senders.load(Ordering::Acquire) == 0 {
            // No more senders: drain once more, then EOF.
            if let Some(v) = self.chan.pop() {
                return Poll::Ready(Some(v));
            }
            return Poll::Ready(None);
        }
        Poll::Pending
    }

    /// Try to receive without waiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.chan.pop() {
            Ok(v)
        } else if self.chan.senders.load(Ordering::Acquire) == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Close the channel: senders start failing, buffered messages can
    /// still be drained.
    pub fn close(&mut self) {
        mark_closed(&self.chan);
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        mark_closed(&self.chan);
    }
}

fn mark_closed<T>(chan: &Arc<Chan<T>>) {
    if !chan.closed.swap(true, Ordering::AcqRel) {
        if let Some(sem) = &chan.cap {
            sem.close();
        }
        chan.on_close.notify_waiters();
    }
}

// ----- unbounded -----

/// Sender half of an unbounded channel.
pub struct UnboundedSender<T> {
    chan: Arc<Chan<T>>,
}

/// Receiver half of an unbounded channel.
pub struct UnboundedReceiver<T> {
    chan: Arc<Chan<T>>,
}

/// Create an unbounded channel.
pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let chan = Arc::new(Chan {
        queue: SegQueue::new(),
        cap: None,
        recv_waker: AtomicWaker::new(),
        senders: AtomicUsize::new(1),
        closed: AtomicBool::new(false),
        on_close: Notify::new(),
    });
    (
        UnboundedSender { chan: chan.clone() },
        UnboundedReceiver { chan },
    )
}

impl<T> UnboundedSender<T> {
    /// Send without waiting (capacity is unbounded).
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.chan.closed.load(Ordering::Acquire) {
            return Err(SendError(value));
        }
        self.chan.queue.push(value);
        self.chan.recv_waker.wake();
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.chan.closed.load(Ordering::Acquire)
    }

    pub async fn closed(&self) {
        loop {
            if self.chan.closed.load(Ordering::Acquire) {
                return;
            }
            let n = self.chan.on_close.notified();
            if self.chan.closed.load(Ordering::Acquire) {
                return;
            }
            n.await;
        }
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.senders.fetch_add(1, Ordering::AcqRel);
        UnboundedSender {
            chan: self.chan.clone(),
        }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        if self.chan.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.chan.recv_waker.wake();
        }
    }
}

impl<T> UnboundedReceiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if let Some(v) = self.chan.pop() {
            return Poll::Ready(Some(v));
        }
        self.chan.recv_waker.register(cx.waker());
        if let Some(v) = self.chan.pop() {
            return Poll::Ready(Some(v));
        }
        if self.chan.senders.load(Ordering::Acquire) == 0 {
            if let Some(v) = self.chan.pop() {
                return Poll::Ready(Some(v));
            }
            return Poll::Ready(None);
        }
        Poll::Pending
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(v) = self.chan.pop() {
            Ok(v)
        } else if self.chan.senders.load(Ordering::Acquire) == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn close(&mut self) {
        mark_closed(&self.chan);
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        mark_closed(&self.chan);
    }
}
