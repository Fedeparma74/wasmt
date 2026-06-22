//! One-shot channel — tokio-compatible API over the main-thread-safe
//! `futures::channel::oneshot` (which uses a spinlock internally, never
//! `std::sync::Mutex`, so it does not trap on the main thread).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::oneshot as fut;

/// Sender half of a one-shot channel.
pub struct Sender<T> {
    inner: fut::Sender<T>,
}

/// Receiver half of a one-shot channel.
pub struct Receiver<T> {
    inner: fut::Receiver<T>,
}

/// Error returned by [`Receiver`] when the sender dropped without sending.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct RecvError(());

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel closed")
    }
}
impl std::error::Error for RecvError {}

/// Error returned by [`Receiver::try_recv`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TryRecvError {
    /// No value sent yet, sender still alive.
    Empty,
    /// Sender dropped without sending.
    Closed,
}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryRecvError::Empty => f.write_str("channel empty"),
            TryRecvError::Closed => f.write_str("channel closed"),
        }
    }
}
impl std::error::Error for TryRecvError {}

/// Create a one-shot channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = fut::channel();
    (Sender { inner: tx }, Receiver { inner: rx })
}

impl<T> Sender<T> {
    /// Send `value`. Returns `Err(value)` if the receiver was dropped.
    pub fn send(self, value: T) -> Result<(), T> {
        self.inner.send(value)
    }

    /// `true` if the receiver has been dropped or closed.
    pub fn is_closed(&self) -> bool {
        self.inner.is_canceled()
    }

    /// Resolves when the receiver is dropped or closed.
    pub async fn closed(&mut self) {
        self.inner.cancellation().await
    }
}

impl<T> Receiver<T> {
    /// Try to receive without waiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        match self.inner.try_recv() {
            Ok(Some(v)) => Ok(v),
            Ok(None) => Err(TryRecvError::Empty),
            Err(_) => Err(TryRecvError::Closed),
        }
    }

    /// Prevent the sender from sending; future `send`s fail.
    pub fn close(&mut self) {
        self.inner.close();
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner)
            .poll(cx)
            .map_err(|_| RecvError(()))
    }
}
