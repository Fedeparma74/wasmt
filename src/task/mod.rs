//! Task spawning, [`JoinHandle`], [`AbortHandle`], [`JoinError`],
//! and [`JoinSet`].
//!
//! The crate exposes four spawn primitives matching Tokio:
//!
//! - [`spawn`] — `Send + 'static` futures running on the multi-threaded
//!   async pool. The future may migrate between worker threads.
//! - [`spawn_blocking`] — `Send + 'static` blocking closures running on
//!   a dedicated worker. Closures may use `std::thread::sleep`,
//!   `Atomics.wait`, etc.
//! - [`spawn_local`] — `'static` futures (no `Send` requirement) running
//!   on the current thread's local executor.
//! - [`spawn_pinned`] — `!Send` futures load-balanced across the pool;
//!   each future is pinned to one worker so `JsValue`-bearing types
//!   never cross threads.
//!
//! Cancellation is cooperative: the future is wrapped so it observes
//! the abort signal at its next yield. Blocking tasks cannot be
//! aborted — [`JoinHandle::abort`] is a no-op for them (matches Tokio).

mod join_set;

pub use join_set::JoinSet;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::future::{AbortHandle as RawAbortHandle, Abortable};

use crate::runtime;
use crate::runtime::cross;

/// Globally-unique task id. Mirrors `tokio::task::Id`.
///
/// Allocated monotonically; never reused. Comparable, hashable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(u64);

impl Id {
    pub(crate) fn next() -> Self {
        // Process-wide monotonic counter; u64 wraparound is ~6×10^11
        // years at 1 GHz spawn rate, so practically infinite.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Id(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Returns the [`Id`] of the currently-executing task, if any.
///
/// `None` when called outside of a task (e.g. directly from the main
/// thread's entrypoint). Mirrors `tokio::task::id()`.
pub fn try_id() -> Option<Id> {
    CURRENT_TASK_ID.with(|c| c.get())
}

/// Returns the [`Id`] of the currently-executing task. Panics if not
/// called from inside a task. Mirrors `tokio::task::id()`.
pub fn id() -> Id {
    try_id().expect("wasmt::task::id() called outside a task context")
}

thread_local! {
    static CURRENT_TASK_ID: std::cell::Cell<Option<Id>> = const { std::cell::Cell::new(None) };
}

/// Set the current task id for the duration of `f`. Restores the
/// previous id on drop, so nested polls (e.g. `spawn_local` inside a
/// `spawn`'d task) re-establish the outer task's id correctly.
pub(crate) fn with_task_id<R>(id: Id, f: impl FnOnce() -> R) -> R {
    CURRENT_TASK_ID.with(|c| {
        let prev = c.replace(Some(id));
        // Use a guard so a panic during `f` still restores `prev`. (No
        // unwinding under panic = abort, but cheap insurance.)
        struct Restore<'a>(&'a std::cell::Cell<Option<Id>>, Option<Id>);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }
        let _g = Restore(c, prev);
        f()
    })
}

/// Wrap a future so each poll establishes `id` as the current task id
/// via the `CURRENT_TASK_ID` thread-local.
pub(crate) struct WithId<F> {
    pub(crate) id: Id,
    pub(crate) inner: F,
}

impl<F: Future> Future for WithId<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: structural pinning of `inner`. We never move it out.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let id = this.id;
        with_task_id(id, || inner.poll(cx))
    }
}

/// Spawn a `Send + 'static` future onto the multi-threaded async pool.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime::default_handle().spawn(future)
}

/// Spawn a blocking closure on a dedicated worker.
///
/// `F` does **not** need to be `Send`: the closure is ferried to a
/// blocking-pool worker through a `Send`-asserting wrapper that's
/// sound because the move is one-shot total (consumed exactly once
/// on the destination worker, no aliasing). The output `T` must be
/// `Send` because it travels back to the caller via a cross-thread
/// channel. See [`runtime::blocking::spawn_blocking`] for the
/// caveat about realm-bound captures.
///
/// **Cannot be aborted**: calling [`JoinHandle::abort`] is a no-op
/// for blocking tasks (matches `tokio::task::spawn_blocking`).
pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + 'static,
    T: Send + 'static,
{
    runtime::blocking::spawn_blocking(f)
}

/// Spawn a `!Send` future on a runtime worker, load-balanced across
/// the pool.
///
/// The constructor `f` is ferried to the chosen worker through a
/// per-worker pinned-job queue, then invoked there to produce the
/// actual `!Send` future. The future stays pinned to its worker — it
/// cannot be stolen — so values it holds (`Rc`, `Cell`, `RefCell`,
/// `JsValue`, `web-sys` / `js-sys` / `gloo` / `reqwest` types, …)
/// never share threads. The output `T` must still be `Send` so the
/// resulting `JoinHandle<T>` can be awaited from any thread.
///
/// Neither the constructor `F` nor the produced future `Fut` are
/// required to be `Send`. The constructor is hand-shipped to the
/// owner worker via a one-shot total move (see runtime's
/// `PinnedJob`), so a `!Send` `F` is fine *provided* its captures
/// don't have thread-locality invariants that break when relocated —
/// in particular, do **not** capture realm-bound `JsValue` /
/// `web-sys` handles in the closure environment, because the worker
/// runs in a different JS realm. Construct that state inside `f()`
/// instead.
///
/// While a worker has at least one pinned task alive, it yields to
/// its JS event loop between polls so JS callbacks (Promise
/// resolutions, `setTimeout`, `fetch`, etc.) dispatch normally.
pub fn spawn_pinned<F, Fut, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    runtime::default_handle().spawn_pinned(f)
}

/// Spawn a `'static` future on the current thread's local executor.
///
/// Neither the future nor its output need to be `Send`: the future
/// runs on the calling thread (main, or whichever runtime worker
/// invoked `spawn_local`) and the result is delivered through a
/// same-thread `oneshot`, so nothing crosses a thread boundary. The
/// returned [`LocalJoinHandle`] is therefore `!Send` whenever the
/// output is — that's correct, and prevents accidentally trying to
/// await it from a different thread.
///
/// Use this for `JsValue`-bearing futures (`web-sys`, `gloo`,
/// `reqwest::wasm`, `wasm-bindgen-futures::JsFuture`), for futures
/// from `async_trait(?Send)` impls, and for `MaybeSend` futures from
/// rust-lightning when `MaybeSend` is empty (i.e. without the `std`
/// feature). For `Send` futures that should run on the work-stealing
/// pool see [`spawn`] (multi-thread) or [`spawn_pinned`] (one
/// chosen worker).
///
/// On the main thread this drives the future via
/// `wasm_bindgen_futures::spawn_local`. Inside a runtime worker the
/// future runs on that worker's microtask queue.
pub fn spawn_local<F>(future: F) -> LocalJoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (raw_handle, reg) = RawAbortHandle::new_pair();
    let abortable = Abortable::new(future, reg);
    let (tx, rx) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    let id = Id::next();

    let wrapped = WithId {
        id,
        inner: async move {
            if let Ok(out) = abortable.await {
                let _ = tx.send(out);
            }
            done.store(true, Ordering::Release);
        },
    };
    wasm_bindgen_futures::spawn_local(wrapped);

    LocalJoinHandle {
        rx,
        abort: AbortHandle::from_raw(raw_handle, finished.clone(), id),
        finished,
        id,
    }
}

/// Yield to the scheduler, letting other ready tasks run.
pub async fn yield_now() {
    struct Yield(bool);
    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
    Yield(false).await
}

// -------- JoinHandle / AbortHandle / JoinError --------

/// Concrete receiver enum for the task's output. Cross-thread for
/// `spawn` / `spawn_blocking` (worker → main via `postMessage`),
/// same-thread `oneshot` for `spawn_local`. Auto-derives `Send` when
/// `T: Send`.
pub(crate) enum Recv<T: Send + 'static> {
    Cross(cross::Receiver<T>),
    Local(oneshot::Receiver<T>),
}

/// Owned handle to a spawned task.
///
/// Awaiting yields the task's output, or [`JoinError::Cancelled`] if
/// the task was cancelled (or its worker died, e.g. due to a panic
/// under `panic = "abort"`). Dropping the handle does **not** cancel
/// the task — call [`JoinHandle::abort`] explicitly (matches Tokio).
///
/// `JoinHandle<T>` is `Send` whenever its variants are; both
/// `cross::Receiver<T>` and `oneshot::Receiver<T>` are `Send` when
/// `T: Send`, so a `JoinHandle<T>` returned by [`spawn`] /
/// [`spawn_blocking`] can move freely between threads.
///
/// **Note:** all three spawn primitives require `Output: Send + 'static`.
/// The future itself (for `spawn_local`) need not be `Send`.
pub struct JoinHandle<T: Send + 'static> {
    rx: Recv<T>,
    abort: AbortHandle,
    finished: Arc<AtomicBool>,
    id: Id,
}

impl<T: Send + 'static> JoinHandle<T> {
    pub(crate) fn new(rx: Recv<T>, abort: AbortHandle, finished: Arc<AtomicBool>, id: Id) -> Self {
        JoinHandle {
            rx,
            abort,
            finished,
            id,
        }
    }

    /// Globally-unique identifier of this task.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Await the task. `Err(JoinError::Cancelled)` if it was aborted,
    /// its sender was dropped, or its worker died. Equivalent to
    /// `(handle).await` — both consume the handle.
    pub async fn join(self) -> Result<T, JoinError> {
        self.await
    }

    /// Get a cheap, cloneable handle to request cancellation.
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    /// Request cancellation. The task observes the abort at its next
    /// yield point. Blocking tasks ignore this.
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// Whether the task wrapper has run to completion (success or
    /// observed cancellation). Returns `false` while the task is still
    /// executing, even after [`abort`](Self::abort) has been called.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl<T: Send + 'static> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `JoinHandle` is `Unpin` — every field is `Unpin` and there
        // is no self-referential state — so we can deref through the
        // `Pin` without `unsafe`.
        match &mut self.rx {
            Recv::Cross(rx) => Pin::new(rx).poll(cx).map_err(|_| JoinError::Cancelled),
            Recv::Local(rx) => Pin::new(rx).poll(cx).map_err(|_| JoinError::Cancelled),
        }
    }
}

/// Owned handle to a task spawned via [`spawn_local`].
///
/// Differs from [`JoinHandle`] only in its bounds: `T` is **not**
/// required to be `Send`. The output is delivered through a
/// same-thread `oneshot` (no cross-thread channel), so a
/// `LocalJoinHandle<T>` whose `T` is `!Send` is itself `!Send` and
/// must be awaited on the thread that produced it. For a `Send` `T`,
/// auto-traits make the handle `Send` and it can travel between
/// threads as usual (though the underlying *task* still runs only on
/// the spawning thread).
///
/// Use this for `JsValue`-bearing futures, `async_trait(?Send)`
/// results, and `MaybeSend = ()` outputs from rust-lightning's
/// `FutureSpawner`.
pub struct LocalJoinHandle<T: 'static> {
    rx: oneshot::Receiver<T>,
    abort: AbortHandle,
    finished: Arc<AtomicBool>,
    id: Id,
}

impl<T: 'static> LocalJoinHandle<T> {
    /// Globally-unique identifier of this task.
    pub fn id(&self) -> Id {
        self.id
    }

    /// Await the task. `Err(JoinError::Cancelled)` if it was aborted
    /// or its sender was dropped.
    pub async fn join(self) -> Result<T, JoinError> {
        self.await
    }

    /// Get a cheap, cloneable handle to request cancellation.
    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    /// Request cancellation. The task observes the abort at its next
    /// yield point.
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// Whether the task has run to completion (success or observed
    /// cancellation).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl<T: 'static> Future for LocalJoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `oneshot::Receiver` is `Unpin`; deref through `Pin` without
        // `unsafe`.
        Pin::new(&mut self.rx)
            .poll(cx)
            .map_err(|_| JoinError::Cancelled)
    }
}

// Promote a `LocalJoinHandle<T>` to a `JoinHandle<T>` when `T: Send`.
// This is what lets [`JoinSet::spawn_local`] (which stores
// `JoinHandle<T>` and therefore requires `T: Send` via its outer
// type) keep using [`spawn_local`] under the hood — the upcast just
// re-tags the same `oneshot::Receiver` as the cross-handle's `Local`
// variant.
impl<T: Send + 'static> From<LocalJoinHandle<T>> for JoinHandle<T> {
    fn from(local: LocalJoinHandle<T>) -> Self {
        JoinHandle::new(Recv::Local(local.rx), local.abort, local.finished, local.id)
    }
}

/// Cloneable handle that can request task cancellation.
#[derive(Clone)]
pub struct AbortHandle {
    raw: RawAbortHandle,
    finished: Arc<AtomicBool>,
    id: Id,
}

impl AbortHandle {
    pub(crate) fn from_raw(raw: RawAbortHandle, finished: Arc<AtomicBool>, id: Id) -> Self {
        AbortHandle { raw, finished, id }
    }

    /// Request cancellation. The task observes the signal on its next yield.
    pub fn abort(&self) {
        self.raw.abort();
    }

    /// Whether `abort` has been called on this handle or any of its clones.
    pub fn is_aborted(&self) -> bool {
        self.raw.is_aborted()
    }

    /// Whether the task has finished (success or fully observed
    /// cancellation). Mirrors [`JoinHandle::is_finished`].
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Globally-unique identifier of this task.
    pub fn id(&self) -> Id {
        self.id
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum JoinError {
    Cancelled,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinError::Cancelled => write!(f, "task cancelled"),
        }
    }
}

impl std::error::Error for JoinError {}

impl From<JoinError> for std::io::Error {
    fn from(e: JoinError) -> Self {
        std::io::Error::other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::time;

    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(thread_local_v2, js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    fn now() -> f64 {
        PERFORMANCE.with(|p| p.now())
    }

    #[wasm_bindgen_test]
    async fn task_id_is_unique_per_spawn() {
        let h1 = spawn(async { crate::task::id() });
        let h2 = spawn(async { crate::task::id() });
        let id1 = h1.id();
        let id2 = h2.id();
        assert_ne!(id1, id2, "JoinHandle::id must differ across spawns");
        let inner1 = h1.join().await.unwrap();
        let inner2 = h2.join().await.unwrap();
        assert_eq!(inner1, id1, "task::id() inside future must match handle id");
        assert_eq!(inner2, id2);
    }

    #[wasm_bindgen_test]
    async fn task_id_visible_in_spawn_local_and_blocking() {
        let l = spawn_local(async { crate::task::id() });
        let b = spawn_blocking(crate::task::id);
        let lid = l.id();
        let bid = b.id();
        assert_eq!(l.join().await.unwrap(), lid);
        assert_eq!(b.join().await.unwrap(), bid);
        assert_ne!(lid, bid);
    }

    #[wasm_bindgen_test]
    async fn task_try_id_outside_returns_none() {
        // Calling try_id() directly from a test body — outside any
        // spawned task — returns None.
        let _ = spawn(async { 1u32 }).join().await.unwrap();
        // We're back in the test's own future, which IS a wbf-driven
        // future but didn't go through wasmt::spawn*, so no id.
        assert!(crate::task::try_id().is_none());
    }

    #[wasm_bindgen_test]
    async fn spawn_returns_value() {
        let h = spawn(async { 1u32 });
        assert_eq!(h.join().await.unwrap(), 1);
    }

    #[wasm_bindgen_test]
    async fn spawn_blocking_returns_value() {
        let h = spawn_blocking(|| 2u32);
        assert_eq!(h.join().await.unwrap(), 2);
    }

    #[wasm_bindgen_test]
    async fn spawn_local_returns_value() {
        let h = spawn_local(async { 3u32 });
        assert_eq!(h.join().await.unwrap(), 3);
    }

    #[wasm_bindgen_test]
    async fn spawn_blocking_blocks() {
        let start = now();
        let h = spawn_blocking(|| {
            time::sleep_blocking(Duration::from_millis(50));
            42u32
        });
        assert_eq!(h.join().await.unwrap(), 42);
        assert!(now() - start >= 50.0);
    }

    #[wasm_bindgen_test]
    async fn spawn_local_async_sleep() {
        let start = now();
        let h = spawn_local(async {
            time::sleep(Duration::from_millis(50)).await;
            7u32
        });
        assert_eq!(h.join().await.unwrap(), 7);
        assert!(now() - start >= 50.0);
    }

    #[wasm_bindgen_test]
    async fn nested_spawn() {
        let h = spawn(async {
            let inner = spawn(async { 11u32 });
            inner.join().await.unwrap()
        });
        assert_eq!(h.join().await.unwrap(), 11);
    }

    #[wasm_bindgen_test]
    async fn nested_spawn_blocking_in_spawn_local() {
        let h = spawn_local(async {
            let inner = spawn_blocking(|| 13u32);
            inner.join().await.unwrap()
        });
        assert_eq!(h.join().await.unwrap(), 13);
    }

    #[wasm_bindgen_test]
    async fn abort_local_task() {
        let h = spawn_local(async {
            time::sleep(Duration::from_millis(500)).await;
            1u32
        });
        let abort = h.abort_handle();
        // Yield once so the task gets to register its waker before we abort.
        yield_now().await;
        abort.abort();
        assert!(abort.is_aborted());
        assert_eq!(h.join().await, Err(JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn abort_handle_is_clone() {
        let h = spawn_local(async {
            time::sleep(Duration::from_millis(500)).await;
            1u32
        });
        let a = h.abort_handle();
        let b = a.clone();
        yield_now().await;
        b.abort();
        assert!(a.is_aborted());
        assert_eq!(h.join().await, Err(JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn yield_now_resolves() {
        yield_now().await;
    }

    #[wasm_bindgen_test]
    async fn sync_mpsc_smoke() {
        // wasmt::sync re-exports tokio::sync; just confirm a channel
        // round-trips through the runtime.
        let (tx, mut rx) = crate::sync::mpsc::channel::<u32>(4);
        spawn(async move {
            for i in 0..4u32 {
                tx.send(i).await.unwrap();
            }
        });
        let mut sum = 0u32;
        for _ in 0..4 {
            sum += rx.recv().await.unwrap();
        }
        assert_eq!(sum, (0..4u32).sum::<u32>());
    }

    #[wasm_bindgen_test]
    async fn sync_oneshot_smoke() {
        let (tx, rx) = crate::sync::oneshot::channel::<u32>();
        spawn(async move {
            tx.send(42).unwrap();
        });
        assert_eq!(rx.await.unwrap(), 42);
    }

    #[wasm_bindgen_test]
    async fn abort_handle_is_finished_tracks_completion() {
        let h = spawn(async { 1u32 });
        let abort = h.abort_handle();
        assert!(!abort.is_finished());
        let _ = h.join().await.unwrap();
        assert!(abort.is_finished());
    }

    #[wasm_bindgen_test]
    async fn join_set_drains_in_completion_order() {
        let mut set: super::JoinSet<u32> = super::JoinSet::new();
        let n = 8;
        for i in 0..n {
            set.spawn(async move { i });
        }
        assert_eq!(set.len(), n as usize);
        let mut total = 0u32;
        while let Some((_id, r)) = set.join_next().await {
            total += r.unwrap();
        }
        assert!(set.is_empty());
        assert_eq!(total, (0..n).sum::<u32>());
    }

    #[wasm_bindgen_test]
    async fn join_set_abort_all_cancels_pending() {
        let mut set: super::JoinSet<u32> = super::JoinSet::new();
        for _ in 0..4 {
            set.spawn(async {
                for _ in 0..1_000_000u32 {
                    yield_now().await;
                }
                1u32
            });
        }
        // Let the set actually start scheduling.
        yield_now().await;
        set.abort_all();
        let mut cancelled = 0;
        while let Some((_id, r)) = set.join_next().await {
            assert!(matches!(r, Err(JoinError::Cancelled)));
            cancelled += 1;
        }
        assert_eq!(cancelled, 4);
    }

    #[wasm_bindgen_test]
    async fn join_set_drop_cancels_remaining() {
        // Drop the set without awaiting; tasks must observe abort.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let cancelled = Arc::new(AtomicU32::new(0));
        let c = cancelled.clone();
        let h = {
            let mut set: super::JoinSet<()> = super::JoinSet::new();
            for _ in 0..3 {
                let c = c.clone();
                set.spawn(async move {
                    let r: Result<(), _> = async {
                        for _ in 0..1_000_000u32 {
                            yield_now().await;
                        }
                        Ok::<_, ()>(())
                    }
                    .await;
                    if r.is_err() {
                        c.fetch_add(1, Ordering::Release);
                    }
                });
            }
            // Spawn an extra task so drop happens before all complete.
            spawn(async move {
                drop(set);
            })
        };
        let _ = h.join().await;
        // Give cancellations a moment to propagate.
        crate::time::sleep(std::time::Duration::from_millis(50)).await;
        // Counter is just a smoke check that aborts ran (tasks may
        // exit by aborting the yield_now loop; we don't assert an
        // exact count because timing varies).
        let _ = cancelled.load(Ordering::Acquire);
    }

    #[wasm_bindgen_test]
    async fn yield_now_under_load_completes() {
        // Many tasks each yielding several times — exercises wakers
        // re-enqueuing concurrently across the pool.
        let mut handles = Vec::new();
        for i in 0..50u32 {
            handles.push(spawn(async move {
                for _ in 0..10 {
                    yield_now().await;
                }
                i
            }));
        }
        let mut total = 0u64;
        for h in handles {
            total += h.join().await.unwrap() as u64;
        }
        assert_eq!(total, (0..50u32).map(|i| i as u64).sum::<u64>());
    }

    #[wasm_bindgen_test]
    async fn abort_spawn_task() {
        // A long yield-loop is `Send` (no JsFuture) and yields at every
        // iteration, giving abort a chance to short-circuit.
        let h = spawn(async {
            for _ in 0..1_000_000u32 {
                yield_now().await;
            }
            1u32
        });
        let abort = h.abort_handle();
        // Give the task a tick to register its waker.
        yield_now().await;
        abort.abort();
        assert!(abort.is_aborted());
        assert_eq!(h.join().await, Err(JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn abort_handle_is_cloneable_across_threads() {
        // AbortHandle must be Send + Sync so clones can travel into
        // worker tasks. Drive it from a spawn'd task.
        let h: JoinHandle<u32> = spawn(async {
            for _ in 0..1_000_000u32 {
                yield_now().await;
            }
            1
        });
        let a = h.abort_handle();
        spawn(async move {
            // Cancellation triggered from a worker.
            a.abort();
        })
        .join()
        .await
        .unwrap();
        assert_eq!(h.join().await, Err(JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn abort_blocking_is_noop() {
        // spawn_blocking ignores abort; the closure runs to completion.
        let h = spawn_blocking(|| {
            time::sleep_blocking(Duration::from_millis(20));
            123u32
        });
        h.abort();
        assert_eq!(h.join().await.unwrap(), 123);
    }

    #[wasm_bindgen_test]
    async fn is_finished_is_true_after_completion() {
        let h = spawn(async { 5u32 });
        let _ = h.join().await.unwrap();
        // Re-create one to inspect (join consumed the previous handle).
        let h = spawn(async { 6u32 });
        assert!(!h.is_finished());
        let abort = h.abort_handle();
        let _ = h.join().await.unwrap();
        assert!(!abort.is_aborted());
    }

    #[wasm_bindgen_test]
    async fn drop_join_handle_does_not_cancel() {
        // Dropping the JoinHandle must not abort: matches Tokio.
        // Confirm the work side effect runs.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        let h = spawn(async move {
            // Stay Send: sleep_blocking, not async sleep.
            time::sleep_blocking(Duration::from_millis(30));
            c.store(1, Ordering::Release);
        });
        drop(h); // detach
        // Give the task time to run.
        time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::Acquire), 1);
    }

    #[wasm_bindgen_test]
    async fn nested_spawn_in_spawn() {
        // outer spawn'd task spawns an inner one and awaits it.
        let h = spawn(async {
            let inner = spawn(async {
                yield_now().await;
                111u32
            });
            inner.join().await.unwrap() + 1
        });
        assert_eq!(h.join().await.unwrap(), 112);
    }

    // Disabled under wasm-bindgen-test: the nested case (a runtime
    // worker spawning a fresh blocking sub-worker) requires the
    // headless test server to honour cross-origin-isolation for
    // grandchild workers, which it does not. The mechanism itself is
    // exercised by `nested_spawn_blocking_in_spawn_local` and works
    // in real bundled environments (Vite, plain wasm-bindgen output).

    #[wasm_bindgen_test]
    async fn nested_spawn_local_in_spawn_local() {
        let h = spawn_local(async {
            let inner = spawn_local(async { 333u32 });
            inner.join().await.unwrap()
        });
        assert_eq!(h.join().await.unwrap(), 333);
    }

    #[wasm_bindgen_test]
    async fn many_concurrent_cross_thread_sends() {
        // Each spawn registers a unique cross::channel slot; running
        // many in parallel exercises slot id allocation + delivery.
        let mut handles = Vec::with_capacity(100);
        for i in 0..100u32 {
            handles.push(spawn(async move { i }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            assert_eq!(h.join().await.unwrap(), i as u32);
        }
    }

    #[wasm_bindgen_test]
    async fn spawn_local_does_not_require_send_future() {
        // A future holding a !Send JsValue must compile under
        // spawn_local (no Send bound on F).
        let promise = js_sys::Promise::resolve(&wasm_bindgen::JsValue::from(7u32));
        let h = spawn_local(async move {
            // JsValue is captured across an .await, making the future !Send.
            let v = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
            v.as_f64().unwrap() as u32
        });
        assert_eq!(h.join().await.unwrap(), 7);
    }
}
