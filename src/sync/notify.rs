//! `Notify` — a main-thread-safe reimplementation of
//! `tokio::sync::Notify`.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;

use super::spin::Spin;

const WAITING: u8 = 0;
const NOTIFIED: u8 = 1;
const CANCELLED: u8 = 2;

struct Waiter {
    state: AtomicU8,
    waker: AtomicWaker,
}

struct Inner {
    /// A stored `notify_one` that arrived with no waiter present.
    permit: bool,
    waiters: VecDeque<Arc<Waiter>>,
}

/// Notify a single task. Mirrors `tokio::sync::Notify`.
///
/// `notify_one` wakes one waiter, or stores a permit if none is
/// waiting so the next `notified().await` returns immediately.
/// `notify_waiters` wakes every current waiter but stores no permit.
pub struct Notify {
    inner: Spin<Inner>,
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl Notify {
    pub const fn new() -> Self {
        Notify {
            inner: Spin::new(Inner {
                permit: false,
                waiters: VecDeque::new(),
            }),
        }
    }

    /// Wake one waiting task, or store a permit for the next waiter.
    pub fn notify_one(&self) {
        let mut woken = None;
        {
            let mut g = self.inner.lock();
            while let Some(w) = g.waiters.pop_front() {
                if w.state.swap(NOTIFIED, Ordering::AcqRel) == WAITING {
                    woken = Some(w);
                    break;
                }
                // else cancelled — skip.
            }
            if woken.is_none() {
                g.permit = true;
            }
        }
        if let Some(w) = woken {
            w.waker.wake();
        }
    }

    /// Wake every currently-registered waiter. Does not store a permit.
    pub fn notify_waiters(&self) {
        let mut wake = Vec::new();
        {
            let mut g = self.inner.lock();
            for w in g.waiters.drain(..) {
                if w.state.swap(NOTIFIED, Ordering::AcqRel) == WAITING {
                    wake.push(w);
                }
            }
        }
        for w in wake {
            w.waker.wake();
        }
    }

    /// Wait for a notification.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            waiter: None,
            done: false,
        }
    }
}

/// Future returned by [`Notify::notified`].
pub struct Notified<'a> {
    notify: &'a Notify,
    waiter: Option<Arc<Waiter>>,
    done: bool,
}

// Matches tokio: the future is `!Unpin`-agnostic but we keep it Unpin
// (no self-referential state) so it is ergonomic to poll behind &mut.
impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(());
        }
        match &this.waiter {
            None => {
                let mut g = this.notify.inner.lock();
                if g.permit {
                    g.permit = false;
                    this.done = true;
                    return Poll::Ready(());
                }
                let w = Arc::new(Waiter {
                    state: AtomicU8::new(WAITING),
                    waker: AtomicWaker::new(),
                });
                w.waker.register(cx.waker());
                g.waiters.push_back(w.clone());
                this.waiter = Some(w);
                Poll::Pending
            }
            Some(w) => {
                w.waker.register(cx.waker());
                if w.state.load(Ordering::Acquire) == NOTIFIED {
                    this.done = true;
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Some(w) = self.waiter.take() else {
            return;
        };
        // Decide under the lock so we never race a notifier (both take
        // `inner`). If still WAITING, remove our node from the queue —
        // leaving it would leak under `select!`-style cancel churn where
        // no `notify_*` ever runs to reap it. If it was already
        // NOTIFIED, the notifier popped it and handed us a wakeup we
        // won't consume, so pass it on (matches tokio).
        let pass_on = {
            let mut g = self.notify.inner.lock();
            if w.state.load(Ordering::Acquire) == NOTIFIED {
                true
            } else {
                w.state.store(CANCELLED, Ordering::Release);
                if let Some(pos) = g.waiters.iter().position(|x| Arc::ptr_eq(x, &w)) {
                    g.waiters.remove(pos);
                }
                false
            }
        };
        if pass_on {
            self.notify.notify_one();
        }
    }
}
