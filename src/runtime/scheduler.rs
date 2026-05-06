//! Per-worker scheduler state.
//!
//! Each runtime worker thread owns a [`WorkerCtx`] containing a
//! single-producer/multi-consumer local FIFO deque, a LIFO slot for
//! "next task" (the freshly-woken task; preserves message-passing
//! locality), and metadata. The matching [`crossbeam_deque::Stealer`]s
//! sit in `super::HandleInner::stealers` so other workers can steal
//! from us.
//!
//! While a worker is inside its loop, [`enter`] publishes the
//! `WorkerCtx` pointer to the thread-local `CURRENT_WORKER`; wakers
//! that fire *on* a runtime worker push to its LIFO slot via
//! [`with_current`], otherwise they fall back to the global injector.

use std::cell::Cell;
use std::sync::Arc;

use crossbeam_deque::{Steal, Stealer, Worker};
use crossbeam_queue::SegQueue;

use super::{Handle, Task};

/// Soft cap on a worker's local deque. When [`WorkerCtx::push_local`]
/// would push beyond this length, half the deque is drained into the
/// global injector to make work visible to siblings (which prevents
/// runaway producers from starving the rest of the pool).
pub(crate) const LOCAL_DEQUE_CAP: usize = 256;

/// Per-worker state owned by a single runtime worker thread.
pub(crate) struct WorkerCtx {
    pub handle: Handle,
    pub index: usize,
    pub local: Worker<Arc<Task>>,
    /// Single-task LIFO slot. The freshly-woken task lands here; the
    /// previous occupant (if any) overflows to the local deque.
    pub lifo: Cell<Option<Arc<Task>>>,
    /// Tick counter for periodic injector polling (fairness).
    pub tick: Cell<u32>,
}

impl WorkerCtx {
    pub fn new(handle: Handle, index: usize, local: Worker<Arc<Task>>) -> Self {
        WorkerCtx {
            handle,
            index,
            local,
            lifo: Cell::new(None),
            tick: Cell::new(0),
        }
    }

    /// Push a task onto this worker's LIFO slot, overflowing the
    /// previous slot occupant onto the local deque. If the local
    /// deque is at the soft cap, half of it is drained into
    /// `injector` first so other workers can pick it up — this
    /// prevents one runaway producer from starving the pool.
    pub fn push_local(&self, task: Arc<Task>, injector: &SegQueue<Arc<Task>>) {
        if let Some(prev) = self.lifo.replace(Some(task)) {
            // The owner is the only consumer that pops from `local`,
            // so `len()` here is consistent (it can only shrink due
            // to stealers; if anything we err on the side of pushing
            // a bit more before overflow, which is fine).
            if self.local.len() >= LOCAL_DEQUE_CAP {
                let drain = LOCAL_DEQUE_CAP / 2;
                for _ in 0..drain {
                    match self.local.pop() {
                        Some(t) => injector.push(t),
                        None => break,
                    }
                }
            }
            self.local.push(prev);
        }
    }

    /// True if both LIFO and local deque are empty.
    pub fn local_is_empty(&self) -> bool {
        // SAFETY: `lifo` is a `Cell` and is only accessed from the
        // single thread that owns this `WorkerCtx`. Reading through
        // the raw pointer avoids moving the `Arc` out and back.
        let lifo_empty = unsafe { (*self.lifo.as_ptr()).is_none() };
        lifo_empty && self.local.is_empty()
    }
}

thread_local! {
    /// Pointer to the running [`WorkerCtx`] on this thread, if any.
    /// Set by [`enter`] and cleared when the returned [`EnterGuard`]
    /// drops, so the pointer stays valid across `await` points
    /// throughout the worker's async loop.
    ///
    /// `Cell<Option<*const _>>` instead of `RefCell` because we only
    /// need a single-thread atomic store/load, no nested borrowing.
    /// `Cell::get` is a plain load (no runtime borrow tracking),
    /// shaving cycles off every `with_current` call — and that's per
    /// task wake.
    static CURRENT_WORKER: Cell<Option<*const WorkerCtx>> = const { Cell::new(None) };
}

/// RAII guard that publishes the worker's `WorkerCtx` to the
/// thread-local for the rest of the worker's lifetime. Drop clears
/// the slot.
pub(crate) struct EnterGuard {
    _priv: (),
}

impl Drop for EnterGuard {
    fn drop(&mut self) {
        CURRENT_WORKER.with(|c| c.set(None));
    }
}

/// Publish `ctx` as the running worker on this thread. The returned
/// guard must outlive every subsequent poll / await on this thread.
pub(crate) fn enter(ctx: &WorkerCtx) -> EnterGuard {
    CURRENT_WORKER.with(|c| c.set(Some(ctx as *const _)));
    EnterGuard { _priv: () }
}

/// If the current thread is inside a worker loop, run `f` against its
/// `WorkerCtx`. Returns `None` otherwise (off-pool callers).
pub(crate) fn with_current<R>(f: impl FnOnce(&WorkerCtx) -> R) -> Option<R> {
    CURRENT_WORKER.with(|c| {
        // SAFETY: the pointer was set by `enter` from a `&WorkerCtx`
        // that outlives this call (the [`EnterGuard`] returned by
        // `enter` is held for the lifetime of the worker loop).
        // Single-threaded access only — wakers fire on the same
        // thread that holds the guard.
        c.get().map(|p| f(unsafe { &*p }))
    })
}

/// Try to steal work from a sibling worker. Returns one task on success
/// (others may have been moved into our local deque as a batch).
pub(crate) fn try_steal(ctx: &WorkerCtx, stealers: &[Stealer<Arc<Task>>]) -> Option<Arc<Task>> {
    let n = stealers.len();
    if n <= 1 {
        return None;
    }
    // Probe round-robin starting one past us, so we don't always hit
    // the same neighbour first.
    let start = (ctx.index + 1) % n;
    for offset in 0..n {
        let i = (start + offset) % n;
        if i == ctx.index {
            continue;
        }
        loop {
            match stealers[i].steal_batch_and_pop(&ctx.local) {
                Steal::Success(t) => return Some(t),
                Steal::Empty => break,
                Steal::Retry => continue,
            }
        }
    }
    None
}
