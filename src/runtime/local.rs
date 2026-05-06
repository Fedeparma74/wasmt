//! Per-worker `LocalSet` infrastructure for `!Send` futures.
//!
//! [`crate::spawn_pinned`] runs a future that doesn't have to be
//! `Send` on a runtime worker. Each worker owns a private set of
//! `LocalTask`s; jobs arrive through a per-worker
//! [`crossbeam_queue::SegQueue`] and are constructed *on* the worker
//! so their `!Send` futures never cross thread boundaries.
//!
//! `LocalTask` is unsafe-impl `Send + Sync` because:
//!
//! - The `future` field is only ever accessed by the owner worker
//!   thread; no other thread touches it.
//! - Wakers (which can fire from any thread) only push the `Arc<LocalTask>`
//!   into the owner's ready queue and notify the owner via the
//!   parking primitive. They never read or mutate `future`.

use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::task::ArcWake;

use super::Handle;

/// `!Send` future the worker drives on its private executor.
pub(crate) type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

pub(crate) struct LocalTask {
    /// `None` once the future polls to completion. Owner-only access.
    future: UnsafeCell<Option<LocalFuture>>,
    owner: usize,
    handle: Handle,
    /// `true` iff this task is currently sitting in the owner worker's
    /// `pinned_ready` queue. Cleared just before [`poll_local`] polls
    /// the future, set by [`ArcWake::wake_by_ref`]. Prevents a task
    /// that wakes itself rapidly (e.g. a `JsFuture` chain firing
    /// many times before the worker resumes) from piling unbounded
    /// `Arc<LocalTask>` clones into `pinned_ready`.
    scheduled: AtomicBool,
}

// SAFETY: see module docs. Wakers never touch `future`; only the
// owner worker thread reads or mutates it.
unsafe impl Send for LocalTask {}
unsafe impl Sync for LocalTask {}

impl LocalTask {
    pub fn new(future: LocalFuture, owner: usize, handle: Handle) -> Arc<Self> {
        Arc::new(LocalTask {
            future: UnsafeCell::new(Some(future)),
            owner,
            handle,
            // Will be enqueued immediately by the constructor's
            // `wake_local_task` call; start as already-scheduled so
            // the first wake doesn't double-enqueue.
            scheduled: AtomicBool::new(true),
        })
    }

    fn clear_scheduled(&self) {
        self.scheduled.store(false, Ordering::Release);
    }
}

impl ArcWake for LocalTask {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if !arc_self.scheduled.swap(true, Ordering::AcqRel) {
            arc_self
                .handle
                .wake_local_task(arc_self.owner, arc_self.clone());
        }
    }
}

/// Drive `task` once. Caller must be on the owner worker thread.
///
/// SAFETY: `task.future` is only accessed from the owner worker; this
/// function must only be called from there.
pub(crate) fn poll_local(task: Arc<LocalTask>) {
    // Clear `scheduled` BEFORE polling so wakes during poll re-enqueue.
    task.clear_scheduled();
    let waker = futures::task::waker_ref(&task);
    let mut cx = Context::from_waker(&waker);
    let fut_slot = unsafe { &mut *task.future.get() };
    let Some(fut) = fut_slot.as_mut() else {
        return;
    };
    if let Poll::Ready(()) = fut.as_mut().poll(&mut cx) {
        *fut_slot = None;
    }
}
