//! Counting semaphore — the foundation for [`super::Mutex`],
//! [`super::RwLock`], and the [`super::mpsc`] capacity bound.
//!
//! Built on the non-parking [`Spin`] lock so it is safe to use from the
//! main thread (see [`super::spin`]). Waiters are served strictly FIFO.
//! Permits are granted all-or-nothing: a waiter requesting `n` permits
//! is only woken once `n` are simultaneously available, which keeps the
//! per-waiter state to a single atomic and avoids partial-grant
//! bookkeeping while preserving fairness.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;

use super::spin::Spin;

// Waiter lifecycle states. All transitions out of `WAITING` happen
// under the semaphore's spin lock, so they never race each other.
const WAITING: u8 = 0;
const GRANTED: u8 = 1;
const CANCELLED: u8 = 2;
const CLOSED: u8 = 3;

struct Waiter {
    needed: usize,
    state: AtomicU8,
    waker: AtomicWaker,
}

struct Inner {
    permits: usize,
    waiters: VecDeque<Arc<Waiter>>,
    closed: bool,
}

/// A counting semaphore. Mirrors `tokio::sync::Semaphore`.
pub struct Semaphore {
    inner: Spin<Inner>,
}

/// Error returned when acquiring from a closed [`Semaphore`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct AcquireError(());

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("semaphore closed")
    }
}
impl std::error::Error for AcquireError {}

/// Error returned by [`Semaphore::try_acquire`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TryAcquireError {
    Closed,
    NoPermits,
}

impl fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed => f.write_str("semaphore closed"),
            TryAcquireError::NoPermits => f.write_str("no permits available"),
        }
    }
}
impl std::error::Error for TryAcquireError {}

impl Semaphore {
    /// The maximum number of permits a `Semaphore` can hold, matching
    /// `tokio::sync::Semaphore::MAX_PERMITS`.
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    pub fn new(permits: usize) -> Self {
        Semaphore {
            inner: Spin::new(Inner {
                permits,
                waiters: VecDeque::new(),
                closed: false,
            }),
        }
    }

    /// Currently-available permits.
    pub fn available_permits(&self) -> usize {
        self.inner.lock().permits
    }

    /// Add `n` permits to the semaphore, waking any waiters that can
    /// now be satisfied.
    pub fn add_permits(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut wake = Vec::new();
        {
            let mut g = self.inner.lock();
            g.permits += n;
            drain_grants(&mut g, &mut wake);
        }
        for w in wake {
            w.wake();
        }
    }

    /// Try to acquire `n` permits without waiting.
    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let mut g = self.inner.lock();
        if g.closed {
            return Err(TryAcquireError::Closed);
        }
        let n = n as usize;
        // Only succeed immediately if no one is already queued ahead of
        // us (preserves FIFO fairness) and enough permits exist.
        if g.waiters.is_empty() && g.permits >= n {
            g.permits -= n;
            Ok(SemaphorePermit {
                sem: self,
                permits: n,
            })
        } else {
            Err(TryAcquireError::NoPermits)
        }
    }

    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    /// Acquire `n` permits, waiting if necessary.
    pub async fn acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, AcquireError> {
        Acquire {
            sem: self,
            needed: n as usize,
            waiter: None,
            done: false,
        }
        .await?;
        Ok(SemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_many(1).await
    }

    /// Acquire an owned permit (holding an `Arc<Semaphore>`).
    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.acquire_many_owned(1).await
    }

    pub async fn acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        Acquire {
            sem: &self,
            needed: n as usize,
            waiter: None,
            done: false,
        }
        .await?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    /// Close the semaphore: all current and future waiters get
    /// [`AcquireError`].
    pub fn close(&self) {
        let mut wake = Vec::new();
        {
            let mut g = self.inner.lock();
            g.closed = true;
            for w in g.waiters.drain(..) {
                if w.state.swap(CLOSED, Ordering::AcqRel) == WAITING {
                    wake.push(w);
                }
            }
        }
        for w in wake {
            w.waker.wake();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }

    /// Return `permits` to the pool (used by permit `Drop`).
    fn release(&self, permits: usize) {
        self.add_permits(permits);
    }
}

/// Walk the FIFO waiter queue front-to-back, granting to each waiter
/// that can be fully satisfied, collecting their wakers to fire after
/// the lock is dropped. Skips (and removes) waiters cancelled while
/// still queued.
fn drain_grants(g: &mut Inner, wake: &mut Vec<Arc<Waiter>>) {
    while let Some(front) = g.waiters.front() {
        match front.state.load(Ordering::Acquire) {
            CANCELLED => {
                g.waiters.pop_front();
            }
            WAITING => {
                if g.permits >= front.needed {
                    let w = g.waiters.pop_front().unwrap();
                    g.permits -= w.needed;
                    // CAS guards against a concurrent cancel (which also
                    // holds the lock, so this can only observe WAITING).
                    w.state.store(GRANTED, Ordering::Release);
                    wake.push(w);
                } else {
                    break;
                }
            }
            // GRANTED/CLOSED shouldn't sit at the front (they're popped
            // at transition time); be defensive and drop them.
            _ => {
                g.waiters.pop_front();
            }
        }
    }
}

impl Waiter {
    fn wake(&self) {
        self.waker.wake();
    }
}

struct Acquire<'a> {
    sem: &'a Semaphore,
    needed: usize,
    waiter: Option<Arc<Waiter>>,
    done: bool,
}

impl Future for Acquire<'_> {
    type Output = Result<(), AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `Acquire` is `Unpin` (no self-referential state).
        let this = self.get_mut();
        debug_assert!(!this.done, "Acquire polled after completion");

        match &this.waiter {
            None => {
                // First poll: fast path or enqueue.
                let mut g = this.sem.inner.lock();
                if g.closed {
                    this.done = true;
                    return Poll::Ready(Err(AcquireError(())));
                }
                if g.waiters.is_empty() && g.permits >= this.needed {
                    g.permits -= this.needed;
                    this.done = true;
                    return Poll::Ready(Ok(()));
                }
                let waiter = Arc::new(Waiter {
                    needed: this.needed,
                    state: AtomicU8::new(WAITING),
                    waker: AtomicWaker::new(),
                });
                waiter.waker.register(cx.waker());
                g.waiters.push_back(waiter.clone());
                this.waiter = Some(waiter);
                Poll::Pending
            }
            Some(waiter) => {
                // Register first, then re-check (no lost wakeup).
                waiter.waker.register(cx.waker());
                match waiter.state.load(Ordering::Acquire) {
                    GRANTED => {
                        this.done = true;
                        Poll::Ready(Ok(()))
                    }
                    CLOSED => {
                        this.done = true;
                        Poll::Ready(Err(AcquireError(())))
                    }
                    _ => Poll::Pending,
                }
            }
        }
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Some(waiter) = self.waiter.take() else {
            return;
        };
        let mut wake = Vec::new();
        {
            let mut g = self.sem.inner.lock();
            match waiter.state.load(Ordering::Acquire) {
                GRANTED => {
                    // We were granted permits but never consumed them
                    // (cancelled between grant and observation). Return
                    // them and let the next waiter take them.
                    g.permits += waiter.needed;
                    drain_grants(&mut g, &mut wake);
                }
                WAITING => {
                    // Still queued: remove our node outright. Merely
                    // marking it would leave it counting toward
                    // `waiters.is_empty()`, which gates the FIFO fast
                    // path — a stale node could then make a *later*
                    // acquirer block even though permits are free
                    // (e.g. an `RwLock` reader stuck behind a cancelled
                    // writer). Removing it also bounds memory under
                    // heavy cancel churn (`select!` timeouts).
                    waiter.state.store(CANCELLED, Ordering::Release);
                    if let Some(pos) = g.waiters.iter().position(|w| Arc::ptr_eq(w, &waiter)) {
                        g.waiters.remove(pos);
                    }
                    // Removing a head-of-line blocker may unblock the
                    // waiters behind it.
                    drain_grants(&mut g, &mut wake);
                }
                _ => {}
            }
        }
        for w in wake {
            w.wake();
        }
    }
}

/// A permit borrowed from a [`Semaphore`]. Returns its permits on drop.
pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl SemaphorePermit<'_> {
    /// Drop the permit without returning it to the semaphore.
    pub fn forget(mut self) {
        self.permits = 0;
    }

    /// Merge another permit's count into this one.
    pub fn merge(&mut self, mut other: SemaphorePermit<'_>) {
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn num_permits(&self) -> usize {
        self.permits
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        if self.permits > 0 {
            self.sem.release(self.permits);
        }
    }
}

/// An owned permit (holds an `Arc<Semaphore>`).
pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: usize,
}

impl OwnedSemaphorePermit {
    pub fn forget(mut self) {
        self.permits = 0;
    }

    pub fn merge(&mut self, mut other: OwnedSemaphorePermit) {
        self.permits += other.permits;
        other.permits = 0;
    }

    pub fn num_permits(&self) -> usize {
        self.permits
    }

    /// The semaphore this permit came from.
    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.sem
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        if self.permits > 0 {
            self.sem.release(self.permits);
        }
    }
}
