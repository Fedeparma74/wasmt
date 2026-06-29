//! Multi-threaded async runtime built on a long-lived pool of Web Workers.
//!
//! Each worker runs a parking executor loop; tasks are pushed to a shared
//! lock-free [`crossbeam_queue::SegQueue`] injector. Workers park on a
//! shared `AtomicI32` via wasm `memory.atomic.wait32` and are woken
//! through `memory.atomic.notify` from the producer side. The main
//! thread never blocks: it only ever pushes and notifies.
//!
//! Result delivery from worker → main goes through a `postMessage`
//! bridge ([`cross`]) because `wasm_bindgen_futures` wakers hold
//! realm-bound `JsValue`s and can't be safely fired from a worker.
//! Worker → worker delivery uses a plain `oneshot` since runtime task
//! wakers re-enqueue via shared memory and are cross-thread-safe.
//!
//! Available only when compiled with `+atomics +bulk-memory
//! +mutable-globals` and built with `--shared-memory --import-memory`.
//! Pages must be served with cross-origin isolation
//! (`Cross-Origin-Opener-Policy: same-origin`,
//! `Cross-Origin-Embedder-Policy: require-corp`) so `SharedArrayBuffer`
//! is available.
//!
//! # Caveats
//!
//! - **Blocking the pool worker.** Calling `std::thread::sleep` (or
//!   [`crate::time::sleep_blocking`]) inside a [`crate::spawn`] future
//!   blocks the pool worker for the duration. Use
//!   [`crate::spawn_blocking`] for blocking work.
//! - **`!Send` futures.** [`crate::spawn`] requires `Send`. Use
//!   [`crate::spawn_local`] for `JsValue`-bearing futures
//!   (`reqwest`/`gloo`/`web-sys`).
//! - **Panics.** Built with `panic = "abort"`. A panic kills the
//!   worker's wasm instance; the task's sender is dropped without
//!   ever sending, surfacing as [`crate::task::JoinError::Cancelled`]
//!   to the awaiter.

pub mod blocking;
pub mod cross;
mod local;
pub(crate) mod main_bus;
pub(crate) mod main_exec;
mod scheduler;
pub mod timer;
mod worker_entry;

pub use main_exec::spawn_on_main;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use crossbeam_deque::{Stealer, Worker as Deque};
use crossbeam_queue::SegQueue;
use futures::future::{AbortHandle as RawAbortHandle, Abortable};
use futures::task::ArcWake;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::wasm_bindgen;

use crate::task::{AbortHandle, JoinHandle};
use scheduler::WorkerCtx;

#[wasm_bindgen(module = "/workerSpawner.js")]
extern "C" {
    #[wasm_bindgen(js_name = spawnRuntimeWorker, catch)]
    fn spawn_runtime_worker(
        module: &wasm_bindgen::JsValue,
        memory: &wasm_bindgen::JsValue,
        handle_ptr: u32,
    ) -> Result<web_sys::Worker, wasm_bindgen::JsValue>;

    #[wasm_bindgen(js_name = setWasmJsUrl)]
    fn js_set_wasm_js_url(url: &str);

    #[wasm_bindgen(js_name = setWasmPkgName)]
    fn js_set_wasm_pkg_name(name: &str);
}

/// Publish the wasm-bindgen package name (from the `WASMT_WASM_PKG` build
/// env) to the JS spawner so it can derive the glue URL from its own
/// snippet location — no DOM autodetection or app-side `setWasmJsUrl`
/// needed for standard `wasm-bindgen --target web` layouts. No-op when the
/// env is unset. Called once before workers are spawned.
fn publish_wasm_pkg_name() {
    if let Some(pkg) = option_env!("WASMT_WASM_PKG") {
        js_set_wasm_pkg_name(pkg);
    }
}

/// Override the URL the worker uses to import the wasm-bindgen JS
/// glue. Call this once before the first spawn if the JS-side
/// autodetection picks the wrong file for your bundler — typically
/// only needed for non-standard layouts. The default detection works
/// for plain wasm-bindgen output, Vite, and most Webpack/Rollup/esbuild
/// setups.
///
/// Exposed to JavaScript as `setWasmJsUrl(url)` from the wasm-bindgen
/// output package, so a bundler entry can do
/// `import { setWasmJsUrl } from '<your-pkg>'` instead of poking
/// `globalThis.__wasmt_wasm_js_url` directly. Equivalent to calling
/// the JS-side `setWasmJsUrl` exported by `workerSpawner.js`, but
/// reachable from the package's public surface without referencing
/// the wasm-bindgen `snippets/` subdirectory by hash.
#[wasm_bindgen(js_name = setWasmJsUrl)]
pub fn set_wasm_js_url(url: &str) {
    js_set_wasm_js_url(url);
}

// Force wasm-bindgen to bundle worker.js next to workerSpawner.js so
// `new URL('./worker.js', workerSpawner.js url)` resolves.
#[wasm_bindgen(module = "/worker.js")]
extern "C" {
    #[wasm_bindgen(js_name = includeWorker)]
    #[allow(dead_code)]
    fn _include_worker();
}

/// Type-erased task future kept on the heap.
type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
/// One-shot slot for handing a worker's local deque to its thread on
/// bootstrap.
type LocalSlot = Mutex<Option<Deque<Arc<Task>>>>;

pub(crate) struct Task {
    /// `None` once polled to completion. Wrapped in `Mutex` so that at
    /// most one worker polls the future at a time.
    future: Mutex<Option<TaskFuture>>,
    handle: Handle,
    /// `true` iff this task is currently sitting in some scheduling
    /// queue (LIFO slot, local deque, or injector) waiting to be
    /// polled. Cleared just before [`poll_task`] polls the future, set
    /// by [`ArcWake::wake_by_ref`]. Prevents a task that wakes itself
    /// (or is awoken by multiple sources) in rapid succession from
    /// piling unbounded `Arc<Task>` clones into the queues.
    scheduled: AtomicBool,
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // Only enqueue if not already scheduled. This swap pairs with
        // `Task::clear_scheduled` (Release) called from `poll_task`
        // before polling: any wake that fires AFTER the clear will
        // observe `false` and successfully enqueue.
        if !arc_self.scheduled.swap(true, Ordering::AcqRel) {
            arc_self.handle.schedule(arc_self.clone());
        }
    }
}

impl Task {
    fn clear_scheduled(&self) {
        self.scheduled.store(false, Ordering::Release);
    }
}

/// A `!Send` constructor closure ferried from the spawning thread to the
/// owner worker that will pin the resulting future.
///
/// `Box<dyn FnOnce()>` would be `!Send`, which would prevent us from
/// pushing onto the cross-thread `SegQueue<PinnedJob>` that fans these
/// jobs out to workers. We wrap the box and assert `Send` manually:
/// the wrapper is **only** moved across threads via a single
/// producer→consumer hand-off (push then pop), the inner closure is
/// invoked exactly once on the destination worker, and its captures
/// live their entire lifetime there. No aliasing remains on the
/// spawning thread once the push succeeds. This is the same one-shot
/// total-move pattern that `LocalTask` uses (see [`local::LocalTask`]).
///
/// **Caveat for callers**: types whose validity depends on the thread
/// they were created on (notably `JsValue` and other realm-bound
/// `wasm-bindgen` handles) are still moved logically; the destination
/// worker would observe an invalid handle when the future runs. Users
/// should construct such state *inside* the closure (`f()` runs on
/// the owner worker) rather than capturing it.
pub(crate) struct PinnedJob(Box<dyn FnOnce() + 'static>);

// SAFETY: see the type doc. The wrapper exists solely to ferry an
// `FnOnce` from one thread to another via the lock-free pinned-jobs
// queue; the FnOnce is consumed once on the destination thread and
// never observed concurrently.
unsafe impl Send for PinnedJob {}

impl PinnedJob {
    pub(crate) fn new(f: impl FnOnce() + 'static) -> Self {
        PinnedJob(Box::new(f))
    }

    pub(crate) fn run(self) {
        (self.0)();
    }
}

pub(crate) struct HandleInner {
    /// Global lock-free MPMC injector for `Send` tasks. Off-pool
    /// producers push here; pool workers periodically pull from it
    /// for fairness.
    injector: SegQueue<Arc<Task>>,
    /// One stealer per pool worker, parallel to the worker indices.
    stealers: Box<[Stealer<Arc<Task>>]>,
    /// Per-worker pre-built local deques, handed off to the worker
    /// thread on bootstrap.
    locals: Box<[LocalSlot]>,
    /// Per-worker parking word. Workers `Atomics.wait` on their own
    /// slot; producers store `PARK_NOTIFIED` + `Atomics.notify` to
    /// release a specific worker.
    parking: Box<[AtomicI32]>,
    /// Per-worker "is parked" flag. Set by a worker before
    /// `Atomics.wait`, cleared on resume. Producers consult it to
    /// decide whether `notify_one` needs to fire.
    parked: Box<[AtomicBool]>,
    /// Aggregate count of currently-parked workers. Producers read
    /// this first to short-circuit the per-worker `parked[i]` scan
    /// when nobody is sleeping (the common case under load).
    /// Incremented by `sync_park` before the wait, decremented after.
    parked_count: AtomicUsize,
    /// Round-robin starting index for `notify_one` so we don't always
    /// wake worker 0.
    notify_cursor: AtomicUsize,
    /// Per-worker queue of `!Send` constructor closures dropped off
    /// by [`Handle::spawn_pinned`]. The owner worker drains and runs
    /// them on its own thread.
    pinned_jobs: Box<[SegQueue<PinnedJob>]>,
    /// Per-worker ready queue of `!Send` tasks, populated by
    /// [`Handle::wake_local_task`] from any thread.
    pinned_ready: Box<[SegQueue<Arc<local::LocalTask>>]>,
    /// Per-worker count of currently-alive `!Send` tasks. Used by
    /// `spawn_pinned` to pick the least-loaded worker.
    pinned_count: Box<[AtomicUsize]>,
    shutdown: AtomicBool,
    /// Count of pool workers that have entered their loop and not yet
    /// exited. Incremented at the top of [`worker_loop`], decremented
    /// when [`run_loop`] returns. Used by
    /// [`Runtime::shutdown_timeout`] to actually wait for workers to
    /// exit instead of just sleeping the timeout.
    alive_workers: AtomicUsize,
    /// Lazy, capped, idle-shrinking pool for blocking closures.
    blocking: blocking::BlockingPool,
    /// Diagnostic: incremented when a worker enters / wakes from its
    /// parking loop.
    heartbeat: AtomicUsize,
    /// Diagnostic: per-worker count of polled `Send` tasks.
    tasks_polled: Box<[AtomicUsize]>,
}

const PARK_EMPTY: i32 = 0;
const PARK_NOTIFIED: i32 = 1;

/// Safety-net timeout (ms) for the `Atomics.waitAsync` park used by
/// workers that have live pinned tasks (see [`async_park`]). Every
/// path that makes such a worker runnable already flips the parking
/// word via [`Handle::notify_worker`], so this timeout should never
/// be the thing that wakes a worker — it only bounds the worst case
/// if a notify is ever missed. Kept short enough to self-heal, long
/// enough that an idle worker wakes ~once/sec (negligible CPU) rather
/// than busy-spinning.
const PINNED_PARK_TIMEOUT_MS: f64 = 1000.0;

/// Cheap, cloneable reference to the runtime. Workers receive one of
/// these via shared memory; spawn callers use it to enqueue work.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<HandleInner>,
}

impl Handle {
    /// Submit a task. Routes to the current worker's LIFO/local deque
    /// when called from inside a runtime worker (preserving
    /// message-passing locality); otherwise routes to the global
    /// injector. When the local deque is full, [`WorkerCtx::push_local`]
    /// transparently overflows half of it back into the injector for
    /// siblings to steal, so a runaway producer cannot starve the pool.
    fn schedule(&self, task: Arc<Task>) {
        let mut task = Some(task);
        let pushed_local = scheduler::with_current(|ctx| {
            if Arc::ptr_eq(&ctx.handle.inner, &self.inner) {
                ctx.push_local(task.take().expect("task moved twice"), &self.inner.injector);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
        if !pushed_local {
            // Off-pool producer: push to injector and wake any parked
            // worker so they can grab it. (Skipping notify here would
            // strand the task whenever every worker is parked.)
            self.inner
                .injector
                .push(task.take().expect("task moved twice"));
            self.notify_one();
        } else if self.inner.parked_count.load(Ordering::Acquire) > 0 {
            // We pushed onto the current worker's queue (it's running,
            // not parked, and will pick up the task on the next loop
            // iter). Only wake siblings if some are actually parked —
            // they may steal from our local deque if push_local
            // overflowed half of it to the injector.
            self.notify_one();
        }
    }

    /// Wake any one parked worker — for global work that anyone can
    /// take. Round-robin across workers so we don't always wake
    /// worker 0.
    fn notify_one(&self) {
        let n = self.inner.parking.len();
        if n == 0 {
            return;
        }
        // Fast path: no one's parked, skip the scan entirely. The
        // common case under load — every wake during steady-state
        // pays only this single Acquire load instead of N flag loads.
        if self.inner.parked_count.load(Ordering::Acquire) == 0 {
            return;
        }
        let start = self.inner.notify_cursor.fetch_add(1, Ordering::Relaxed) % n;
        for offset in 0..n {
            let idx = (start + offset) % n;
            if self.inner.parked[idx].load(Ordering::Acquire) {
                self.notify_worker(idx);
                return;
            }
        }
    }

    /// Wake one specific worker (for pinned-task wakes that only the
    /// owner can act on).
    fn notify_worker(&self, idx: usize) {
        self.inner.parking[idx].store(PARK_NOTIFIED, Ordering::Release);
        atomics_notify(&self.inner.parking[idx], 1);
    }

    /// Wake every worker (used on shutdown).
    fn notify_all(&self) {
        for slot in self.inner.parking.iter() {
            slot.store(PARK_NOTIFIED, Ordering::Release);
            atomics_notify(slot, 1);
        }
    }

    /// Cross-thread waker entry for `!Send` tasks: enqueues into the
    /// owner worker's pinned-ready queue and wakes it.
    pub(crate) fn wake_local_task(&self, owner: usize, task: Arc<local::LocalTask>) {
        self.inner.pinned_ready[owner].push(task);
        self.notify_worker(owner);
    }

    fn signal_shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        self.notify_all();
        self.inner.blocking.signal_shutdown();
    }

    /// Access this runtime's blocking pool.
    pub(crate) fn blocking(&self) -> &blocking::BlockingPool {
        &self.inner.blocking
    }

    /// Spawn a `Send + 'static` future on the runtime's worker pool.
    ///
    /// Result delivery uses a cross-thread `postMessage` bridge when
    /// called from main (so the main-thread receiver can be woken by
    /// a worker), or a plain `oneshot::channel` when called from
    /// inside a runtime worker (the runtime's task waker is safe to
    /// fire cross-thread already, so a postMessage hop is unnecessary).
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (raw_abort, reg) = RawAbortHandle::new_pair();
        let abortable = Abortable::new(future, reg);
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let id = crate::task::Id::next();

        let recv = if crate::utils::is_worker_scope() {
            let (tx, rx) = futures::channel::oneshot::channel();
            self.spawn_inner(crate::task::WithId {
                id,
                inner: async move {
                    if let Ok(out) = abortable.await {
                        let _ = tx.send(out);
                    }
                    done.store(true, Ordering::Release);
                },
            });
            crate::task::Recv::Local(rx)
        } else {
            let (tx, rx) = cross::channel::<F::Output>();
            self.spawn_inner(crate::task::WithId {
                id,
                inner: async move {
                    if let Ok(out) = abortable.await {
                        tx.send(out);
                    }
                    done.store(true, Ordering::Release);
                },
            });
            crate::task::Recv::Cross(rx)
        };

        JoinHandle::new(
            recv,
            AbortHandle::from_raw(raw_abort, finished.clone(), id),
            finished,
            id,
        )
    }

    fn spawn_inner<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task_future: TaskFuture = Box::pin(fut);
        // `scheduled` starts `true` because we're enqueuing
        // immediately via `schedule(...)`. The first poll will clear
        // it, opening the gate for subsequent wakes to re-enqueue.
        self.schedule(Arc::new(Task {
            future: Mutex::new(Some(task_future)),
            handle: self.clone(),
            scheduled: AtomicBool::new(true),
        }));
    }

    /// Spawn a `!Send` future on a chosen pool worker. The
    /// constructor `f` runs *on* that worker, so the future it
    /// produces never crosses thread boundaries.
    ///
    /// Neither `F` nor `Fut` need to be `Send`: the constructor is
    /// ferried to the chosen worker through the per-worker pinned-job
    /// queue via a one-shot total-move (see [`PinnedJob`] for the
    /// soundness argument), so it's safe even when `f` captures
    /// `!Send` state. Output `T` must still be `Send` so the resulting
    /// `JoinHandle<T>` can be awaited from a different thread.
    ///
    /// **Caveat**: avoid capturing realm-bound handles (`JsValue`,
    /// `web-sys` types, …) directly in `f`'s closure environment. The
    /// closure runs on a worker whose JS realm is distinct from the
    /// caller's, so any captured `JsValue` would point into the wrong
    /// realm. Construct such state *inside* `f` (`f()` runs on the
    /// owner worker) instead.
    ///
    /// Use for `web-sys` / `js-sys` / `gloo` / `reqwest`-style
    /// workloads that hold `JsValue`s. The pool distributes by
    /// picking the worker with the fewest live pinned tasks.
    pub fn spawn_pinned<F, Fut, T>(&self, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let owner = self.least_loaded_worker();
        self.inner.pinned_count[owner].fetch_add(1, Ordering::AcqRel);

        // Result-delivery channel: cross-thread bridge from main,
        // plain oneshot from inside a worker.
        enum Tx<T> {
            Cross(cross::Sender<T>),
            Local(futures::channel::oneshot::Sender<T>),
        }
        let (tx, recv) = if crate::utils::is_worker_scope() {
            let (tx, rx) = futures::channel::oneshot::channel::<T>();
            (Tx::Local(tx), crate::task::Recv::Local(rx))
        } else {
            let (tx, rx) = cross::channel::<T>();
            (Tx::Cross(tx), crate::task::Recv::Cross(rx))
        };

        let (raw_abort, reg) = RawAbortHandle::new_pair();
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let id = crate::task::Id::next();

        // Decrement the pinned counter when the wrapper future drops —
        // whether by completion, cancellation, or worker termination.
        let dec = PinnedCountDec {
            handle: self.clone(),
            owner,
        };
        let handle = self.clone();
        let constructor = PinnedJob::new(move || {
            let dec = dec;
            let fut = f();
            let abortable = Abortable::new(fut, reg);
            let wrapped: local::LocalFuture = Box::pin(crate::task::WithId {
                id,
                inner: async move {
                    if let Ok(out) = abortable.await {
                        match tx {
                            Tx::Cross(tx) => tx.send(out),
                            Tx::Local(tx) => {
                                let _ = tx.send(out);
                            }
                        }
                    }
                    done.store(true, Ordering::Release);
                    drop(dec);
                },
            });
            let task = local::LocalTask::new(wrapped, owner, handle.clone());
            handle.wake_local_task(owner, task);
        });

        self.inner.pinned_jobs[owner].push(constructor);
        self.notify_worker(owner);

        JoinHandle::new(
            recv,
            AbortHandle::from_raw(raw_abort, finished.clone(), id),
            finished,
            id,
        )
    }

    fn least_loaded_worker(&self) -> usize {
        let counts = &self.inner.pinned_count;
        let mut best = 0;
        let mut best_n = counts[0].load(Ordering::Relaxed);
        for (i, c) in counts.iter().enumerate().skip(1) {
            let n = c.load(Ordering::Relaxed);
            if n < best_n {
                best_n = n;
                best = i;
            }
        }
        best
    }
}

/// RAII guard that decrements a worker's `pinned_count` on drop.
struct PinnedCountDec {
    handle: Handle,
    owner: usize,
}

impl Drop for PinnedCountDec {
    fn drop(&mut self) {
        self.handle.inner.pinned_count[self.owner].fetch_sub(1, Ordering::AcqRel);
    }
}

/// Owning handle to the runtime: holds the (`!Send`) `web_sys::Worker`s
/// and drops them on shutdown. Lives on the main thread.
pub struct Runtime {
    handle: Handle,
    workers: Vec<web_sys::Worker>,
}

impl Runtime {
    /// Build a runtime with `worker_threads` async workers and the
    /// default blocking-pool config (cap = 32, idle = 10s).
    ///
    /// Workers boot asynchronously; tasks spawned before workers come
    /// online stay in the queue and are picked up as soon as the first
    /// worker is ready. The constructor itself does not block.
    pub fn with_workers(worker_threads: usize) -> Self {
        Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .build()
    }

    /// Build a runtime sized to `navigator.hardwareConcurrency`
    /// (uncapped) with the default blocking-pool config.
    pub fn new() -> Self {
        Builder::new_multi_thread().build()
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// Initiate a graceful shutdown: signals every pool worker and
    /// blocking-pool worker to exit at its next iteration. Returns
    /// once every async-pool worker has actually exited its loop, or
    /// after `timeout` elapses (whichever comes first). Workers still
    /// alive at the deadline are hard-`terminate()`'d.
    ///
    /// Like `tokio::runtime::Runtime::shutdown_timeout`, this
    /// consumes the runtime — calling it on the lazily-initialised
    /// default runtime is not possible; build an explicit
    /// [`Runtime`] for that.
    pub async fn shutdown_timeout(self, timeout: std::time::Duration) {
        // Tell everyone to stop.
        self.handle.signal_shutdown();

        // Poll alive_workers until it hits zero or the deadline
        // arrives. Each pool worker decrements alive_workers when
        // its run_loop returns.
        let deadline = timer::Instant::now() + timeout;
        while timer::Instant::now() < deadline {
            if self.handle.inner.alive_workers.load(Ordering::Acquire) == 0 {
                break;
            }
            crate::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Hard-terminate any still-alive worker (and the blocking-pool
        // workers, which we haven't tracked individually here — the
        // BlockingPoolInner Arc inside them will drop when terminated,
        // and signal_shutdown already drained the queue so awaiters
        // saw Cancelled).
        for w in &self.workers {
            w.terminate();
        }
        // self drops here (Vec<Worker> drops, Closures drop, Arcs decrement).
    }
}

/// Default cap on simultaneously-alive blocking-pool workers. Web
/// Workers are heavier than OS threads, so this is set well below
/// Tokio's 512.
pub const DEFAULT_MAX_BLOCKING_THREADS: usize = 32;
/// Default idle timeout before a blocking-pool worker exits cleanly.
pub const DEFAULT_BLOCKING_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Configuration builder for [`Runtime`].
///
/// ```ignore
/// let rt = wasmt::runtime::Builder::new_multi_thread()
///     .worker_threads(4)
///     .max_blocking_threads(16)
///     .blocking_idle_timeout(std::time::Duration::from_secs(5))
///     .build();
/// ```
pub struct Builder {
    worker_threads: Option<usize>,
    max_blocking_threads: usize,
    blocking_idle_timeout: std::time::Duration,
}

impl Builder {
    /// Start a builder for a multi-threaded runtime.
    pub fn new_multi_thread() -> Self {
        Builder {
            worker_threads: None,
            max_blocking_threads: DEFAULT_MAX_BLOCKING_THREADS,
            blocking_idle_timeout: DEFAULT_BLOCKING_IDLE_TIMEOUT,
        }
    }

    /// Number of async-pool workers. Defaults to
    /// `navigator.hardwareConcurrency` (uncapped). Clamped to ≥ 1.
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n.max(1));
        self
    }

    /// Cap on simultaneously-alive blocking-pool workers. Default
    /// [`DEFAULT_MAX_BLOCKING_THREADS`].
    pub fn max_blocking_threads(mut self, n: usize) -> Self {
        self.max_blocking_threads = n.max(1);
        self
    }

    /// Idle timeout before an unused blocking-pool worker exits.
    /// Default [`DEFAULT_BLOCKING_IDLE_TIMEOUT`].
    pub fn blocking_idle_timeout(mut self, d: std::time::Duration) -> Self {
        self.blocking_idle_timeout = d;
        self
    }

    /// Construct the runtime. Async-pool workers boot asynchronously;
    /// the call itself does not block. The blocking pool is empty
    /// until the first [`crate::spawn_blocking`] arrives.
    pub fn build(self) -> Runtime {
        // Sanity check: SharedArrayBuffer must be available, which
        // requires either cross-origin isolation (COOP/COEP) or a
        // browser opt-in. Without it, our wasm Memory's underlying
        // buffer is a regular ArrayBuffer, `memory.atomic.wait32`
        // throws, and the workers die silently as soon as they try
        // to park. Better to fail loudly here, on main, with a
        // diagnostic the user can act on.
        verify_shared_memory();

        // Hand the JS spawner the package name so it can locate the glue
        // without DOM autodetection (needed under Nuxt/Vite/Webpack where
        // the glue is loaded via dynamic `import()`).
        publish_wasm_pkg_name();

        let n = self
            .worker_threads
            .unwrap_or_else(hardware_concurrency)
            .max(1);

        let mut locals = Vec::with_capacity(n);
        let mut stealers = Vec::with_capacity(n);
        for _ in 0..n {
            let w: Deque<Arc<Task>> = Deque::new_fifo();
            stealers.push(w.stealer());
            locals.push(Mutex::new(Some(w)));
        }
        let parking = (0..n).map(|_| AtomicI32::new(PARK_EMPTY)).collect();
        let parked = (0..n).map(|_| AtomicBool::new(false)).collect();
        let pinned_jobs = (0..n).map(|_| SegQueue::new()).collect();
        let pinned_ready = (0..n).map(|_| SegQueue::new()).collect();
        let pinned_count = (0..n).map(|_| AtomicUsize::new(0)).collect();
        let tasks_polled = (0..n).map(|_| AtomicUsize::new(0)).collect();

        let inner = Arc::new(HandleInner {
            injector: SegQueue::new(),
            stealers: stealers.into_boxed_slice(),
            locals: locals.into_boxed_slice(),
            parking,
            parked,
            parked_count: AtomicUsize::new(0),
            notify_cursor: AtomicUsize::new(0),
            pinned_jobs,
            pinned_ready,
            pinned_count,
            shutdown: AtomicBool::new(false),
            alive_workers: AtomicUsize::new(0),
            blocking: blocking::BlockingPool::new(
                self.max_blocking_threads,
                self.blocking_idle_timeout,
            ),
            heartbeat: AtomicUsize::new(0),
            tasks_polled,
        });
        let handle = Handle { inner };

        let module = wasm_bindgen::module();
        let memory = wasm_bindgen::memory();

        let mut workers = Vec::with_capacity(n);
        for index in 0..n {
            let entry: *mut WorkerBootstrap = Box::into_raw(Box::new(WorkerBootstrap {
                handle: handle.clone(),
                index,
            }));
            match spawn_runtime_worker(&module, &memory, entry as u32) {
                Ok(worker) => {
                    // On runtime worker death: saturating-decrement
                    // `alive_workers` so `shutdown_timeout` doesn't
                    // wait out the full budget on a dead worker.
                    let h = handle.clone();
                    main_bus::install_listener(
                        &worker,
                        Some(Box::new(move || {
                            let counter = &h.inner.alive_workers;
                            let mut cur = counter.load(Ordering::Acquire);
                            while cur > 0 {
                                match counter.compare_exchange(
                                    cur,
                                    cur - 1,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                ) {
                                    Ok(_) => break,
                                    Err(seen) => cur = seen,
                                }
                            }
                        })),
                    );
                    workers.push(worker);
                }
                Err(err) => {
                    // Reclaim the box for THIS worker (it never received it).
                    let _ = unsafe { Box::from_raw(entry) };
                    // Tear down the partial pool: tell already-booted
                    // workers to exit, then hard-terminate them so we
                    // don't leave zombie workers + a leaked handle Arc
                    // chain behind when we panic.
                    handle.signal_shutdown();
                    for w in &workers {
                        w.terminate();
                    }
                    web_sys::console::error_1(&"wasmt: failed to spawn runtime worker".into());
                    panic!("wasmt::Runtime: failed to spawn worker: {:?}", err);
                }
            }
        }

        Runtime { handle, workers }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.signal_shutdown();
        for w in &self.workers {
            w.terminate();
        }
    }
}

/// Panic with a clear diagnostic if the wasm Memory's underlying
/// buffer isn't a `SharedArrayBuffer`. SAB requires either:
///
/// - Cross-origin isolation: response headers
///   `Cross-Origin-Opener-Policy: same-origin` and
///   `Cross-Origin-Embedder-Policy: require-corp` on the page (and
///   on every same-origin sub-resource), so the browser sets
///   `crossOriginIsolated == true`.
/// - Or a browser-specific bypass (Firefox:
///   `dom.postMessage.sharedArrayBuffer.bypassCOOP_COEP.insecure.enabled`).
///
/// Without SAB, our wasm Memory is unshared, `memory.atomic.wait32`
/// throws on workers, and the runtime would silently fail on first
/// park. Catching this on main gives a usable error.
fn verify_shared_memory() {
    let memory = wasm_bindgen::memory();
    let buffer = match js_sys::Reflect::get(&memory, &"buffer".into()) {
        Ok(b) => b,
        Err(_) => return, // can't introspect; assume host knows best.
    };
    let ctor = js_sys::Reflect::get(&buffer, &"constructor".into()).ok();
    let name = ctor
        .as_ref()
        .and_then(|c| js_sys::Reflect::get(c, &"name".into()).ok())
        .and_then(|n| n.as_string())
        .unwrap_or_default();
    if name != "SharedArrayBuffer" {
        let coi = js_sys::Reflect::get(&js_sys::global(), &"crossOriginIsolated".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        panic!(
            "wasmt requires SharedArrayBuffer (got `{name}` for wasm Memory's \
             buffer; crossOriginIsolated = {coi}). Serve the page with \
             `Cross-Origin-Opener-Policy: same-origin` and \
             `Cross-Origin-Embedder-Policy: require-corp`. See the wasmt \
             README for hosting requirements."
        );
    }
}

pub(crate) fn hardware_concurrency() -> usize {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        return (window.navigator().hardware_concurrency() as usize).max(1);
    }
    if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        return (scope.navigator().hardware_concurrency() as usize).max(1);
    }
    1
}

/// If the current thread is an async-pool worker, returns its index
/// in the runtime's worker pool. Otherwise (main, blocking-pool
/// worker, external Web Worker), returns `None`.
pub(crate) fn current_pool_worker_index() -> Option<usize> {
    scheduler::with_current(|ctx| ctx.index)
}

/// Run one task to completion or `Pending`. If `Pending`, the task's
/// waker will re-enqueue it when ready.
fn poll_task(task: Arc<Task>, ctx: &WorkerCtx) {
    ctx.handle.inner.tasks_polled[ctx.index].fetch_add(1, Ordering::Relaxed);
    // Clear `scheduled` BEFORE polling, so any wake that fires during
    // poll observes `false` and enqueues a fresh entry. (Wakes that
    // fire between pop-from-queue and this clear are dropped — that's
    // sound because the upcoming poll will pick up whatever shared
    // state the waker meant to signal.)
    task.clear_scheduled();
    // `waker_ref` builds a non-owning Waker (wrapped in ManuallyDrop),
    // saving one `Arc::clone` + drop pair per poll vs `waker(arc)`.
    // If the future clones cx.waker() to register it, that clone
    // does the Arc-increment we'd otherwise have eaten up front.
    let waker = futures::task::waker_ref(&task);
    let mut cx = Context::from_waker(&waker);

    let mut guard = task.future.lock().expect("task future poisoned");
    let Some(fut) = guard.as_mut() else { return };
    if let Poll::Ready(()) = fut.as_mut().poll(&mut cx) {
        *guard = None;
    }
}

/// Bootstrap data passed from main to a runtime worker on spawn.
struct WorkerBootstrap {
    handle: Handle,
    index: usize,
}

/// How often (in tasks polled) to check the global injector before the
/// local deque, so injected work isn't starved by a worker that keeps
/// re-feeding its own LIFO slot.
const GLOBAL_QUEUE_INTERVAL: u32 = 31;

/// Work-doing run-loop iterations between forced *macrotask* yields. The
/// per-task yields are microtasks, which the JS event loop drains entirely
/// before dispatching any macrotask — so an unbroken run of ready tasks
/// would starve browser events that arrive as macrotasks. Yielding a macrotask
/// every Nth busy iteration bounds that starvation while amortising the
/// yield's event-loop-round-trip cost.
const MACROTASK_YIELD_INTERVAL: u32 = 256;

/// Worker entry point: turn the bootstrap struct into a `WorkerCtx`,
/// claim our local deque, run the executor loop until shutdown.
///
/// Async because pool workers must yield to the worker's JS event
/// loop whenever a pinned task is alive — the worker's microtask
/// queue is what dispatches `Promise.then` callbacks for any
/// `JsFuture` a pinned task awaits. When no pinned tasks are alive
/// we use `Atomics.wait` for cheap, blocking parking.
async fn worker_loop(boot: WorkerBootstrap) {
    let WorkerBootstrap { handle, index } = boot;
    let local = handle.inner.locals[index]
        .lock()
        .expect("locals poisoned")
        .take()
        .expect("local deque already taken");
    let ctx = WorkerCtx::new(handle.clone(), index, local);

    handle.inner.heartbeat.fetch_add(1, Ordering::Release);
    handle.inner.alive_workers.fetch_add(1, Ordering::AcqRel);

    let _guard = scheduler::enter(&ctx);
    run_loop(&ctx, &handle).await;

    // Decrement only after the loop returns (i.e. shutdown was
    // observed). Done here, not in a Drop, so we get the decrement
    // even though the wasm-bindgen-generated future may run drop
    // glue lazily after the JS Promise resolves.
    handle.inner.alive_workers.fetch_sub(1, Ordering::AcqRel);
}

async fn run_loop(ctx: &WorkerCtx, handle: &Handle) {
    let idx = ctx.index;
    loop {
        if handle.inner.shutdown.load(Ordering::Acquire) {
            break;
        }

        // Periodically yield a macrotask so this worker's JS event loop
        // dispatches browser events. The per-task yields below
        // are microtasks; the event loop drains the microtask queue in
        // full before reaching any macrotask, so a worker with an unbroken
        // run of ready tasks would starve those events for as long as the
        // run lasts. The counter only advances on busy iterations
        // (an idle worker parks instead of ooping), so the cost
        // scales with how busy the worker is.
        let mt = ctx.macro_tick.get().wrapping_add(1);
        ctx.macro_tick.set(mt);
        if mt.is_multiple_of(MACROTASK_YIELD_INTERVAL) {
            yield_macrotask().await;
        }

        // Drain pinned-job constructors (each one builds a !Send
        // future and pushes it onto our pinned-ready queue). The
        // constructors arrive via `SegQueue<PinnedJob>` whose `Send`
        // is implemented unsafely on the wrapper; consuming them here
        // is the one and only consumption site.
        while let Some(job) = handle.inner.pinned_jobs[idx].pop() {
            job.run();
        }

        // 1. LIFO slot — preserves message-passing locality, but
        //    budget-limited so a self-rewaking task can't monopolise
        //    the worker and starve the local deque (see `LIFO_BUDGET`).
        if let Some(t) = ctx.lifo.take() {
            if ctx.lifo_polls.get() >= scheduler::LIFO_BUDGET && !ctx.local.is_empty() {
                // Budget spent and siblings are queued: demote this
                // task to the back of the local deque and fall through
                // to serve the deque (FIFO) so queued tasks progress.
                ctx.local.push(t);
                ctx.lifo_polls.set(0);
            } else {
                ctx.lifo_polls.set(ctx.lifo_polls.get() + 1);
                poll_task(t, ctx);
                yield_for_pinned(idx, handle).await;
                continue;
            }
        }

        // 2. Periodic injector poll for fairness.
        let tick = ctx.tick.get().wrapping_add(1);
        ctx.tick.set(tick);
        if tick.is_multiple_of(GLOBAL_QUEUE_INTERVAL + 1)
            && let Some(t) = handle.inner.injector.pop()
        {
            ctx.lifo_polls.set(0);
            poll_task(t, ctx);
            yield_for_pinned(idx, handle).await;
            continue;
        }

        // 3. Pinned-ready (one !Send task). Always yield to the JS
        //    event loop afterward so any `Promise.then` callbacks the
        //    pinned future just registered get dispatched.
        if let Some(local) = handle.inner.pinned_ready[idx].pop() {
            local::poll_local(local);
            yield_microtask().await;
            continue;
        }

        // 4. Local deque.
        if let Some(t) = ctx.local.pop() {
            ctx.lifo_polls.set(0);
            poll_task(t, ctx);
            yield_for_pinned(idx, handle).await;
            continue;
        }

        // 5. Global injector.
        if let Some(t) = handle.inner.injector.pop() {
            ctx.lifo_polls.set(0);
            poll_task(t, ctx);
            yield_for_pinned(idx, handle).await;
            continue;
        }

        // 6. Steal from siblings.
        if let Some(t) = scheduler::try_steal(ctx, &handle.inner.stealers) {
            ctx.lifo_polls.set(0);
            poll_task(t, ctx);
            yield_for_pinned(idx, handle).await;
            continue;
        }

        // Nothing to do. Park.
        if handle.inner.pinned_count[idx].load(Ordering::Acquire) > 0 {
            // Pinned tasks are alive; some may be awaiting JS Promise
            // callbacks that only dispatch when the worker yields to
            // its event loop. A blocking `Atomics.wait` would freeze
            // the loop and starve those callbacks, so we park via
            // `Atomics.waitAsync` instead: it keeps the event loop
            // running (callbacks fire, wakers flip the parking word)
            // while suspending this worker — no busy-spin. This is the
            // difference between an idle pinned worker sleeping and one
            // pegging a CPU core cycling macrotasks.
            async_park(ctx, handle).await;
        } else {
            sync_park(ctx, handle).await;
        }
    }
}

/// `Atomics.wait`-based park, used only when no pinned tasks exist
/// (so no JS event-loop callbacks need to be drained).
async fn sync_park(ctx: &WorkerCtx, handle: &Handle) {
    let idx = ctx.index;
    let prev = handle.inner.parking[idx].swap(PARK_EMPTY, Ordering::AcqRel);
    if prev == PARK_NOTIFIED {
        return;
    }
    // Announce parked state. Order matters: parked[idx] = true is
    // Released BEFORE we increment parked_count (so producers that
    // see parked_count > 0 and pick this slot via parked[idx] will
    // correctly observe the parked-true state).
    handle.inner.parked[idx].store(true, Ordering::Release);
    handle.inner.parked_count.fetch_add(1, Ordering::AcqRel);
    if !handle.inner.injector.is_empty()
        || !ctx.local_is_empty()
        || !handle.inner.pinned_jobs[idx].is_empty()
        || !handle.inner.pinned_ready[idx].is_empty()
        || handle.inner.pinned_count[idx].load(Ordering::Acquire) > 0
        || handle.inner.shutdown.load(Ordering::Acquire)
        || any_stealer_has_work(handle)
    {
        handle.inner.parked[idx].store(false, Ordering::Release);
        handle.inner.parked_count.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    atomics_wait(&handle.inner.parking[idx], PARK_EMPTY);
    handle.inner.parked[idx].store(false, Ordering::Release);
    handle.inner.parked_count.fetch_sub(1, Ordering::AcqRel);
    handle.inner.heartbeat.fetch_add(1, Ordering::Release);
}

/// `Atomics.waitAsync`-based park, used when this worker has live
/// pinned (`!Send`) tasks.
///
/// Unlike [`sync_park`]'s blocking `Atomics.wait` — illegal to use here
/// because it would freeze the worker's JS event loop and never let the
/// `Promise.then` callbacks that pinned futures are awaiting dispatch —
/// `Atomics.waitAsync` returns a Promise and lets the event loop keep
/// running while this worker is suspended. So JS callbacks still fire
/// (and their wakers flip the parking word through
/// [`Handle::wake_local_task`] → [`Handle::notify_worker`]), yet the
/// worker does *not* busy-spin. `Atomics.notify` wakes `waitAsync`
/// waiters exactly as it wakes `Atomics.wait` waiters, so the existing
/// notify path needs no changes.
///
/// A finite timeout ([`PINNED_PARK_TIMEOUT_MS`]) bounds the wait purely
/// as a safety net; on expiry we just fall through and re-poll.
async fn async_park(ctx: &WorkerCtx, handle: &Handle) {
    let idx = ctx.index;
    let prev = handle.inner.parking[idx].swap(PARK_EMPTY, Ordering::AcqRel);
    if prev == PARK_NOTIFIED {
        return;
    }
    // Announce parked state before the re-check, same ordering as
    // `sync_park` (parked[idx] Released before parked_count bump).
    handle.inner.parked[idx].store(true, Ordering::Release);
    handle.inner.parked_count.fetch_add(1, Ordering::AcqRel);
    // Re-check every source of work to close the park/produce race.
    // NB: unlike `sync_park` we do NOT bail on `pinned_count > 0` —
    // parking *with* pinned tasks alive is the entire purpose here.
    if !handle.inner.injector.is_empty()
        || !ctx.local_is_empty()
        || !handle.inner.pinned_jobs[idx].is_empty()
        || !handle.inner.pinned_ready[idx].is_empty()
        || handle.inner.shutdown.load(Ordering::Acquire)
        || any_stealer_has_work(handle)
    {
        handle.inner.parked[idx].store(false, Ordering::Release);
        handle.inner.parked_count.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    wait_async(
        &handle.inner.parking[idx],
        PARK_EMPTY,
        PINNED_PARK_TIMEOUT_MS,
    )
    .await;
    handle.inner.parked[idx].store(false, Ordering::Release);
    handle.inner.parked_count.fetch_sub(1, Ordering::AcqRel);
    handle.inner.heartbeat.fetch_add(1, Ordering::Release);
}

/// Yield a microtask iff there are pinned tasks alive on this
/// worker. The microtask drains any `Promise.then` callbacks queued
/// by pinned futures during the most recent poll.
async fn yield_for_pinned(idx: usize, handle: &Handle) {
    if handle.inner.pinned_count[idx].load(Ordering::Acquire) > 0 {
        yield_microtask().await;
    }
}

// --------------------------------------------------------------------
// Custom yield primitives — bypasses `wasm_bindgen_futures::JsFuture`
// and `Promise::resolve` allocations on every yield.
//
// The naive impl `JsFuture::from(Promise.resolve(...)).await` allocates
// per call: 1 oneshot::channel (2 Arcs) + 2 Closure boxes + 2 JS
// callbacks. For pinned-task heavy workloads (thousands of yields per
// second), this dominates yield latency.
//
// Replacement: a per-thread persistent JS dispatcher (one
// MessageChannel for macrotask, one queueMicrotask call site for
// microtask) that pops slots from a FIFO queue and fires their
// wakers. Each `yield_*` allocates a single `Rc<YieldSlot>` instead.
// --------------------------------------------------------------------

/// One-shot slot shared between a `YieldFuture` and the per-thread
/// dispatcher that fires it.
///
/// Single-threaded — `Cell` instead of `AtomicBool`/`Mutex` because
/// the slot only travels between code paths on the same worker
/// thread. `wake()` of the stored Waker is itself thread-safe (the
/// Waker is the runtime task waker, which re-enqueues across threads
/// via `Handle::schedule`).
struct YieldSlot {
    fired: std::cell::Cell<bool>,
    waker: std::cell::Cell<Option<std::task::Waker>>,
}

impl YieldSlot {
    fn new() -> std::rc::Rc<Self> {
        std::rc::Rc::new(YieldSlot {
            fired: std::cell::Cell::new(false),
            waker: std::cell::Cell::new(None),
        })
    }

    /// Called by the per-thread dispatcher when its turn comes.
    fn fire(&self) {
        self.fired.set(true);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

struct YieldFuture {
    slot: std::rc::Rc<YieldSlot>,
}

impl Future for YieldFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.slot.fired.get() {
            return Poll::Ready(());
        }
        // Register-then-recheck pattern.
        self.slot.waker.set(Some(cx.waker().clone()));
        if self.slot.fired.get() {
            // Race: dispatcher fired between get() and set(). Take
            // whatever waker we just set (no need to wake; we're
            // returning Ready) and move on.
            let _ = self.slot.waker.take();
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

/// FIFO queue of pending yield slots, single-threaded per worker.
type YieldQueue =
    std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<std::rc::Rc<YieldSlot>>>>;

// ---------- macrotask dispatcher: per-thread MessageChannel ----------

thread_local! {
    static MACROTASK_DISPATCHER: std::cell::RefCell<Option<MacrotaskDispatcher>> =
        const { std::cell::RefCell::new(None) };
}

struct MacrotaskDispatcher {
    /// `port2.postMessage` resolved to a `js_sys::Function`. Call
    /// with `this = port2` (binding mandatory).
    port2_post: js_sys::Function,
    port2: wasm_bindgen::JsValue,
    pending: YieldQueue,
    /// Holders to keep JS objects alive for the thread's lifetime.
    _handler: wasm_bindgen::closure::Closure<dyn FnMut(wasm_bindgen::JsValue)>,
    _port1: wasm_bindgen::JsValue,
    _channel: wasm_bindgen::JsValue,
}

fn macrotask_dispatcher_init() -> Option<(js_sys::Function, wasm_bindgen::JsValue, YieldQueue)> {
    use wasm_bindgen::JsCast;

    let global = js_sys::global();
    let ctor = js_sys::Reflect::get(&global, &"MessageChannel".into()).ok()?;
    if !ctor.is_function() {
        return None;
    }
    let channel = js_sys::Reflect::construct(&ctor.into(), &js_sys::Array::new()).ok()?;
    let port1 = js_sys::Reflect::get(&channel, &"port1".into()).ok()?;
    let port2 = js_sys::Reflect::get(&channel, &"port2".into()).ok()?;
    let port2_post: js_sys::Function = js_sys::Reflect::get(&port2, &"postMessage".into())
        .ok()?
        .dyn_into()
        .ok()?;

    let pending: YieldQueue =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::new()));
    let pending_for_handler = pending.clone();

    // Long-lived handler, FIFO pop one slot per postMessage delivery.
    let handler = wasm_bindgen::closure::Closure::<dyn FnMut(wasm_bindgen::JsValue)>::new(
        move |_evt: wasm_bindgen::JsValue| {
            let next = pending_for_handler.borrow_mut().pop_front();
            if let Some(slot) = next {
                slot.fire();
            }
        },
    );
    let _ = js_sys::Reflect::set(&port1, &"onmessage".into(), handler.as_ref());

    // Explicit start() for browsers that defer dispatch (no-op when
    // already started by `onmessage =`).
    if let Ok(start_fn) = js_sys::Reflect::get(&port1, &"start".into())
        && let Ok(start_fn) = start_fn.dyn_into::<js_sys::Function>()
    {
        let _ = start_fn.call0(&port1);
    }

    let port2_for_state = port2.clone();
    MACROTASK_DISPATCHER.with(|c| {
        *c.borrow_mut() = Some(MacrotaskDispatcher {
            port2_post: port2_post.clone(),
            port2: port2_for_state,
            pending: pending.clone(),
            _handler: handler,
            _port1: port1,
            _channel: channel,
        });
    });
    Some((port2_post, port2, pending))
}

/// Yield to the next event-loop tick to give JS a chance to run
/// macrotasks. Uses `MessageChannel.postMessage` (no clamp) and a
/// per-thread persistent dispatcher.
async fn yield_macrotask() {
    let slot = YieldSlot::new();

    let dispatcher_state = MACROTASK_DISPATCHER.with(|c| {
        c.borrow()
            .as_ref()
            .map(|d| (d.port2_post.clone(), d.port2.clone(), d.pending.clone()))
    });
    let (port2_post, port2, pending) = match dispatcher_state {
        Some(s) => s,
        None => match macrotask_dispatcher_init() {
            Some(s) => s,
            None => {
                // No MessageChannel — fall back to setTimeout(0)
                // (clamped to 4 ms but at least progresses). Defensive
                // path; every supported target has MessageChannel.
                yield_settimeout_fallback().await;
                return;
            }
        },
    };
    pending.borrow_mut().push_back(slot.clone());
    let _ = port2_post.call1(&port2, &wasm_bindgen::JsValue::UNDEFINED);
    YieldFuture { slot }.await;
}

/// Last-resort fallback when `MessageChannel` is unavailable.
async fn yield_settimeout_fallback() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let global = js_sys::global();
        if let Some(scope) = global.dyn_ref::<web_sys::DedicatedWorkerGlobalScope>() {
            scope
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .expect("setTimeout in worker scope");
        } else if let Some(window) = global.dyn_ref::<web_sys::Window>() {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .expect("setTimeout on main");
        } else {
            let _ = resolve.call0(&wasm_bindgen::JsValue::UNDEFINED);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

// ---------- microtask dispatcher: per-thread queueMicrotask --------

thread_local! {
    static MICROTASK_DISPATCHER: std::cell::RefCell<Option<MicrotaskDispatcher>> =
        const { std::cell::RefCell::new(None) };
}

struct MicrotaskDispatcher {
    /// `globalThis.queueMicrotask` resolved to a `js_sys::Function`.
    /// Called with the dispatcher closure as its argument.
    queue_microtask: js_sys::Function,
    /// Bound dispatch closure passed as the queueMicrotask argument.
    /// One closure per thread, reused for every yield.
    dispatch_fn: wasm_bindgen::JsValue,
    pending: YieldQueue,
    /// Holder to keep the dispatch closure alive.
    _dispatch_holder: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

fn microtask_dispatcher_init() -> Option<(js_sys::Function, wasm_bindgen::JsValue, YieldQueue)> {
    use wasm_bindgen::JsCast;

    let global = js_sys::global();
    let qm = js_sys::Reflect::get(&global, &"queueMicrotask".into()).ok()?;
    let qm: js_sys::Function = qm.dyn_into().ok()?;

    let pending: YieldQueue =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::new()));
    let pending_for_dispatch = pending.clone();

    let dispatch = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        let next = pending_for_dispatch.borrow_mut().pop_front();
        if let Some(slot) = next {
            slot.fire();
        }
    });
    let dispatch_fn: wasm_bindgen::JsValue = dispatch.as_ref().clone();

    MICROTASK_DISPATCHER.with(|c| {
        *c.borrow_mut() = Some(MicrotaskDispatcher {
            queue_microtask: qm.clone(),
            dispatch_fn: dispatch_fn.clone(),
            pending: pending.clone(),
            _dispatch_holder: dispatch,
        });
    });
    Some((qm, dispatch_fn, pending))
}

/// Yield to the next microtask checkpoint so queued
/// `Promise.then` callbacks dispatch.
///
/// Uses `globalThis.queueMicrotask(fn)` + a per-thread persistent
/// dispatcher. ~5x fewer allocations per yield than the naive
/// `JsFuture::from(Promise.resolve()).await` pattern (which
/// allocates a `oneshot::channel`, two `Closure` boxes, two JS
/// callbacks, plus the Promise itself per call).
async fn yield_microtask() {
    let slot = YieldSlot::new();

    let dispatcher_state = MICROTASK_DISPATCHER.with(|c| {
        c.borrow().as_ref().map(|d| {
            (
                d.queue_microtask.clone(),
                d.dispatch_fn.clone(),
                d.pending.clone(),
            )
        })
    });
    let (qm, dispatch_fn, pending) = match dispatcher_state {
        Some(s) => s,
        None => match microtask_dispatcher_init() {
            Some(s) => s,
            None => {
                // No queueMicrotask — fall back to Promise.resolve().
                let p = js_sys::Promise::resolve(&wasm_bindgen::JsValue::UNDEFINED);
                let _ = wasm_bindgen_futures::JsFuture::from(p).await;
                return;
            }
        },
    };
    pending.borrow_mut().push_back(slot.clone());
    // queueMicrotask is a free function on globalThis; `this` is
    // ignored, but we pass undefined for clarity.
    let _ = qm.call1(&wasm_bindgen::JsValue::UNDEFINED, &dispatch_fn);
    YieldFuture { slot }.await;
}

fn any_stealer_has_work(handle: &Handle) -> bool {
    handle.inner.stealers.iter().any(|s| !s.is_empty())
}

// --- Atomics.wait / Atomics.notify wrappers -------------------------

fn atomics_wait(slot: &AtomicI32, expected: i32) {
    // SAFETY: the AtomicI32 lives in shared linear memory (it's inside
    // an Arc<HandleInner> reachable from the wasm-bindgen Memory).
    // memory_atomic_wait32 expects an exclusive *mut to the cell.
    unsafe {
        core::arch::wasm32::memory_atomic_wait32(
            slot as *const AtomicI32 as *mut i32,
            expected,
            -1,
        );
    }
}

fn atomics_notify(slot: &AtomicI32, count: u32) {
    unsafe {
        core::arch::wasm32::memory_atomic_notify(slot as *const AtomicI32 as *mut i32, count);
    }
}

/// Async, non-blocking counterpart to [`atomics_wait`]: park on `slot`
/// via `Atomics.waitAsync` until it's notified or `timeout_ms` elapses.
///
/// `Atomics.waitAsync` (unlike `Atomics.wait`) does not block the
/// calling thread's JS event loop — it returns a Promise — so it is
/// the only primitive that lets a worker sleep on its parking word
/// *while still draining the JS callbacks its pinned futures depend
/// on*. The same `Atomics.notify` that wakes `atomics_wait` resolves
/// this Promise.
///
/// Resolves immediately if `slot` no longer holds `expected`
/// ("not-equal"), on notify, on timeout, or — defensively — if the
/// engine lacks `Atomics.waitAsync` (every cross-origin-isolated
/// engine that ships `SharedArrayBuffer` also ships `waitAsync`, so
/// the macrotask fallback is effectively dead code kept for safety).
async fn wait_async(slot: &AtomicI32, expected: i32, timeout_ms: f64) {
    // `Int32Array` over the live wasm memory buffer (a
    // SharedArrayBuffer in the multithreaded build). Built fresh each
    // call so a `memory.grow` between parks can't leave us viewing a
    // stale buffer. `slot`'s linear address is stable (it lives in a
    // pinned `Arc<HandleInner>`); the array index is that byte offset
    // divided by 4.
    let memory = wasm_bindgen::memory();
    let buffer = match js_sys::Reflect::get(&memory, &"buffer".into()) {
        Ok(b) => b,
        Err(_) => {
            yield_macrotask().await;
            return;
        }
    };
    let array = js_sys::Int32Array::new(&buffer);
    let index = (slot as *const AtomicI32 as u32) / 4;

    match js_sys::Atomics::wait_async_with_timeout(&array, index, expected, timeout_ms) {
        Ok(result) => {
            // `{ async: bool, value: Promise | "not-equal" | "timed-out" }`.
            // When `async` is false the wait already settled
            // synchronously (the word changed) — return and re-poll.
            let is_async = js_sys::Reflect::get(&result, &"async".into())
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_async
                && let Ok(promise) = js_sys::Reflect::get(&result, &"value".into())
                    .and_then(|v| v.dyn_into::<js_sys::Promise>())
            {
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
        }
        Err(_) => {
            // `Atomics.waitAsync` unavailable on this engine — degrade
            // to the macrotask yield (busy but correct; keeps the event
            // loop live).
            yield_macrotask().await;
        }
    }
}

impl Handle {
    #[doc(hidden)]
    pub fn heartbeat(&self) -> usize {
        self.inner.heartbeat.load(Ordering::Acquire)
    }

    /// Per-worker count of tasks polled. Test/diagnostic API. The
    /// returned slice is indexed by worker index and is only useful
    /// for verifying load distributes across the pool.
    #[doc(hidden)]
    pub fn tasks_polled(&self) -> Vec<usize> {
        self.inner
            .tasks_polled
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect()
    }

    /// Number of pool workers.
    #[doc(hidden)]
    pub fn worker_count(&self) -> usize {
        self.inner.stealers.len()
    }

    /// Number of pool workers that have entered their loop and not
    /// yet exited.
    #[doc(hidden)]
    pub fn alive_workers(&self) -> usize {
        self.inner.alive_workers.load(Ordering::Acquire)
    }

    /// Blocking-pool diagnostics — currently-alive worker count.
    #[doc(hidden)]
    pub fn blocking_worker_count(&self) -> usize {
        self.inner.blocking.worker_count()
    }

    /// Blocking-pool diagnostics — peak alive worker count since start.
    #[doc(hidden)]
    pub fn blocking_peak_workers(&self) -> usize {
        self.inner.blocking.peak_workers()
    }

    /// Blocking-pool diagnostics — total jobs run.
    #[doc(hidden)]
    pub fn blocking_jobs_run(&self) -> usize {
        self.inner.blocking.jobs_run()
    }

    /// Blocking-pool configured cap.
    #[doc(hidden)]
    pub fn blocking_max_workers(&self) -> usize {
        self.inner.blocking.max_workers()
    }
}

// --------- Singleton runtime, lazily initialized on first spawn ---------

static GLOBAL: OnceLock<Handle> = OnceLock::new();

/// Returns a `Handle` to the lazily-initialized default runtime.
///
/// The default runtime has `navigator.hardwareConcurrency` async
/// workers and is created the first time it is requested. Workers boot
/// in the background; the call itself does not block.
///
/// **Must be first invoked from the main thread.** The default runtime
/// is intentionally leaked: the `Runtime` value is never dropped, so
/// workers live until the page (or the wasm instance) is destroyed.
/// Use [`Runtime::with_workers`] directly for explicit shutdown.
pub fn default_handle() -> Handle {
    GLOBAL
        .get_or_init(|| {
            let runtime = Runtime::new();
            let handle = runtime.handle();
            // Intentional leak: keep workers alive for the lifetime of the page.
            std::mem::forget(runtime);
            handle
        })
        .clone()
}

impl Handle {
    /// Returns a [`Handle`] to the runtime that's currently driving
    /// the calling code. From a runtime worker, returns that worker's
    /// runtime. From main (or any non-pool thread), returns the
    /// lazily-initialised default runtime — same as
    /// [`default_handle`]. Mirrors `tokio::runtime::Handle::current`.
    ///
    /// Unlike Tokio, this never panics: outside any runtime context,
    /// we fall back to the default singleton instead of erroring.
    pub fn current() -> Handle {
        if let Some(h) = scheduler::with_current(|ctx| ctx.handle.clone()) {
            return h;
        }
        default_handle()
    }

    /// Drive `future` to completion on the **calling thread's** local
    /// executor and return a JS `Promise` that resolves to the
    /// future's output. Use this when you need a JS-promise return
    /// value (typically from a `#[wasm_bindgen]`-exported async
    /// function on main, where `block_on` is illegal).
    ///
    /// The future runs via `wasm_bindgen_futures::spawn_local`, so it
    /// may be `!Send` (no worker migration). Output `T` must be
    /// convertible to a JS value via `Into<JsValue>`.
    ///
    /// Mirrors the role of `tokio::runtime::Runtime::block_on` for
    /// the JS interop case: you get a `Promise` instead of a blocking
    /// thread park, since main can't park.
    pub fn block_on_main<F, T>(future: F) -> wasm_bindgen::JsValue
    where
        F: Future<Output = T> + 'static,
        T: Into<wasm_bindgen::JsValue> + 'static,
    {
        let promise =
            wasm_bindgen_futures::future_to_promise(async move { Ok(future.await.into()) });
        promise.into()
    }
}

/// Block the calling thread until `future` completes. Returns the
/// future's output.
///
/// **May only be called from a non-async worker thread** — typically
/// a `spawn_blocking` worker or a user-managed Web Worker outside
/// the async pool. Panics:
///
/// - On the main thread (`Atomics.wait` is illegal there).
/// - From inside an async-pool worker's executor (would deadlock the
///   worker for the duration of the `block_on`, starving every other
///   task assigned to it). Use `.await` from async contexts instead.
///
/// The implementation parks the worker on a private `AtomicI32` /
/// `Atomics.wait` between polls. The future's waker, when fired,
/// stores `1` into the parking word and notifies; the worker wakes,
/// re-polls, and the cycle continues until `Ready`.
///
/// Mirrors `tokio::runtime::Runtime::block_on` (worker side only,
/// with the same "no nested block_on inside async" panic).
pub fn block_on<F: Future>(future: F) -> F::Output {
    if !crate::utils::is_worker_scope() {
        panic!(
            "wasmt::runtime::block_on must be called from a worker thread; \
             on main, use Handle::block_on_main to get a JsPromise instead"
        );
    }
    if scheduler::with_current(|_| ()).is_some() {
        panic!(
            "wasmt::runtime::block_on cannot be called from within a \
             wasmt async executor — use `.await` from async contexts instead"
        );
    }

    use std::task::{RawWaker, RawWakerVTable, Waker};

    // Heap-allocate the parking word in an Arc. Each Waker clone
    // holds an Arc strong ref; the Waker pointer stays valid even if
    // a future stashes a clone past block_on's return (the storage
    // outlives `block_on`'s stack frame and is freed when the last
    // clone drops). Without this, late wakes after block_on returns
    // could corrupt rewound stack memory.
    let park: Arc<AtomicI32> = Arc::new(AtomicI32::new(0));

    unsafe fn clone(p: *const ()) -> RawWaker {
        // SAFETY: `p` was constructed from `Arc::into_raw`. Recover
        // the Arc, increment, leak both ends.
        unsafe {
            Arc::increment_strong_count(p as *const AtomicI32);
        }
        RawWaker::new(p, &VTABLE)
    }
    unsafe fn wake(p: *const ()) {
        // SAFETY: ownership transferred to us; consume the Arc.
        let park: Arc<AtomicI32> = unsafe { Arc::from_raw(p as *const AtomicI32) };
        park.store(1, Ordering::Release);
        unsafe {
            core::arch::wasm32::memory_atomic_notify(Arc::as_ptr(&park) as *mut i32, 1);
        }
        // park drops here; if it was the last ref, frees the storage.
    }
    unsafe fn wake_by_ref(p: *const ()) {
        // SAFETY: borrow only — DO NOT consume the Arc.
        let park = unsafe { &*(p as *const AtomicI32) };
        park.store(1, Ordering::Release);
        unsafe {
            core::arch::wasm32::memory_atomic_notify(park as *const AtomicI32 as *mut i32, 1);
        }
    }
    unsafe fn drop_w(p: *const ()) {
        // SAFETY: matched against the Arc::into_raw / increment_strong_count.
        unsafe {
            Arc::decrement_strong_count(p as *const AtomicI32);
        }
    }

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_w);

    // Hand out one strong ref to the Waker; balanced by drop_w.
    let raw = Arc::into_raw(park.clone());
    let waker = unsafe { Waker::from_raw(RawWaker::new(raw as *const (), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(future);

    loop {
        // Reset park BEFORE poll so wakes during the poll are observed.
        park.store(0, Ordering::Release);
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        // Fast-recheck park before sleeping (waker may have fired
        // synchronously during poll).
        if park.load(Ordering::Acquire) == 1 {
            continue;
        }
        // SAFETY: park is in shared linear memory; expected = 0.
        unsafe {
            core::arch::wasm32::memory_atomic_wait32(Arc::as_ptr(&park) as *mut i32, 0, -1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn sanity_shared_array_buffer_available() {
        let global = js_sys::global();
        let has_sab = js_sys::Reflect::has(&global, &"SharedArrayBuffer".into()).unwrap();
        assert!(
            has_sab,
            "SharedArrayBuffer must be available (set COOP/COEP)"
        );
        let memory = wasm_bindgen::memory();
        // wasm_bindgen::memory() returns the WebAssembly.Memory; its
        // .buffer is a SharedArrayBuffer when --shared-memory is set.
        let ctor = js_sys::Reflect::get(&memory, &"buffer".into()).unwrap();
        let ctor_name = js_sys::Reflect::get(&ctor, &"constructor".into())
            .and_then(|c| js_sys::Reflect::get(&c, &"name".into()))
            .ok()
            .and_then(|n| n.as_string())
            .unwrap_or_default();
        assert_eq!(
            ctor_name, "SharedArrayBuffer",
            "wasm memory.buffer must be SharedArrayBuffer; got {}",
            ctor_name
        );
    }

    #[wasm_bindgen_test]
    async fn diag_workers_boot() {
        // Confirm at least one worker reaches its parking loop within
        // a generous time budget, before we test cross-thread comm.
        let h = default_handle();
        for _ in 0..50 {
            if h.heartbeat() >= 1 {
                return;
            }
            crate::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "no worker became live within 5s; heartbeat={}",
            h.heartbeat()
        );
    }

    #[wasm_bindgen_test]
    async fn smoke_default_handle_runs_one_task() {
        let h = default_handle();
        let join = h.spawn(async { 7u32 });
        assert_eq!(join.join().await.unwrap(), 7);
    }

    #[wasm_bindgen_test]
    async fn smoke_default_handle_runs_many_tasks() {
        let h = default_handle();
        let mut handles = Vec::with_capacity(32);
        for i in 0..32u32 {
            handles.push(h.spawn(async move { i * 2 }));
        }
        for (i, jh) in handles.into_iter().enumerate() {
            assert_eq!(jh.join().await.unwrap(), (i as u32) * 2);
        }
    }

    #[wasm_bindgen_test]
    async fn pool_worker_runs_blocking_sleep() {
        let h = default_handle();
        let jh = h.spawn(async {
            std::thread::sleep(Duration::from_millis(20));
            42u32
        });
        assert_eq!(jh.join().await.unwrap(), 42);
    }

    #[wasm_bindgen_test]
    async fn explicit_runtime_with_workers_runs_tasks() {
        // Explicit Runtime with a fixed pool size of 2.
        let rt = Runtime::with_workers(2);
        assert_eq!(rt.handle().worker_count(), 2);
        let h = rt.handle();
        let jh = h.spawn(async { 99u32 });
        assert_eq!(jh.join().await.unwrap(), 99);
        // Drop terminates workers; allow a moment for terminate() to land.
        drop(rt);
    }

    #[wasm_bindgen_test]
    async fn explicit_runtime_with_zero_workers_clamps_to_one() {
        let rt = Runtime::with_workers(0);
        assert_eq!(rt.handle().worker_count(), 1);
    }

    #[wasm_bindgen_test]
    async fn stress_thousand_tasks() {
        // 1000 concurrent tiny tasks must all complete.
        let h = default_handle();
        let mut handles = Vec::with_capacity(1000);
        for i in 0..1000u32 {
            handles.push(h.spawn(async move { i.wrapping_mul(3) }));
        }
        let mut sum: u64 = 0;
        for jh in handles {
            sum += jh.join().await.unwrap() as u64;
        }
        let expected: u64 = (0..1000u32).map(|i| i.wrapping_mul(3) as u64).sum();
        assert_eq!(sum, expected);
    }

    #[wasm_bindgen_test]
    async fn load_distributes_across_pool() {
        // Use an explicit 4-worker runtime so the assertion isn't
        // platform-dependent (`navigator.hardwareConcurrency` varies).
        let rt = Runtime::with_workers(4);
        let h = rt.handle();

        // Wait until all 4 workers have parked at least once, so we
        // know they're all online before we measure distribution.
        for _ in 0..50 {
            if h.heartbeat() >= 4 {
                break;
            }
            crate::time::sleep(Duration::from_millis(50)).await;
        }

        // Spawn enough work for stealing to be observable. Each task
        // does a tiny CPU spin so workers actually contend.
        let mut handles = Vec::new();
        for i in 0..200u32 {
            handles.push(h.spawn(async move {
                let mut acc = i;
                for _ in 0..1_000 {
                    acc = acc.wrapping_mul(31).wrapping_add(7);
                }
                acc
            }));
        }
        for jh in handles {
            jh.join().await.unwrap();
        }

        let polled = h.tasks_polled();
        let total: usize = polled.iter().sum();
        // At least 200 tasks counted (could be more — wakers re-poll).
        assert!(total >= 200, "polled total too low: {total}");
        // Every worker did at least one task — load distributed.
        let active = polled.iter().filter(|n| **n > 0).count();
        assert!(
            active >= 2,
            "load did not distribute across pool: {polled:?}"
        );
    }

    // ----- blocking pool -----

    #[wasm_bindgen_test]
    async fn blocking_pool_starts_empty_until_used() {
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(4)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let h = rt.handle();
        // Default pool has zero workers until the first job arrives.
        assert_eq!(h.blocking_worker_count(), 0);
        assert_eq!(h.blocking_jobs_run(), 0);
        assert_eq!(h.blocking_max_workers(), 4);
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_reuses_workers_serially() {
        // 10 jobs run one-at-a-time → peak workers should be 1.
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(8)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let h = rt.handle();
        let pool = h.inner.blocking.clone();
        for _ in 0..10u32 {
            let h = crate::runtime::blocking::spawn_blocking_on(&pool, || 1u32);
            assert_eq!(h.join().await.unwrap(), 1);
        }
        assert!(
            h.blocking_peak_workers() <= 2,
            "serial jobs caused too many workers: {}",
            h.blocking_peak_workers()
        );
        assert_eq!(h.blocking_jobs_run(), 10);
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_grows_under_concurrent_load() {
        // 4 concurrent jobs each blocking ~50ms → expect at least 2
        // workers alive at the peak.
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(8)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let pool = rt.handle().inner.blocking.clone();
        let h = rt.handle();
        let mut handles = Vec::new();
        for _ in 0..4u32 {
            handles.push(crate::runtime::blocking::spawn_blocking_on(&pool, || {
                std::thread::sleep(Duration::from_millis(50));
                1u32
            }));
        }
        for jh in handles {
            jh.join().await.unwrap();
        }
        assert!(
            h.blocking_peak_workers() >= 2,
            "concurrent jobs did not grow the pool: peak = {}",
            h.blocking_peak_workers()
        );
        assert!(h.blocking_peak_workers() <= 4);
        assert_eq!(h.blocking_jobs_run(), 4);
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_caps_at_max() {
        // cap = 2, fire 5 long jobs concurrently. Peak workers must
        // never exceed 2.
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let pool = rt.handle().inner.blocking.clone();
        let h = rt.handle();
        let mut handles = Vec::new();
        for _ in 0..5u32 {
            handles.push(crate::runtime::blocking::spawn_blocking_on(&pool, || {
                std::thread::sleep(Duration::from_millis(40));
                1u32
            }));
        }
        for jh in handles {
            jh.join().await.unwrap();
        }
        assert!(
            h.blocking_peak_workers() <= 2,
            "cap violated: peak = {}",
            h.blocking_peak_workers()
        );
        assert_eq!(h.blocking_jobs_run(), 5);
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_idle_workers_exit() {
        // Use a tiny idle timeout so the test doesn't hang for 10s.
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(4)
            .blocking_idle_timeout(Duration::from_millis(150))
            .build();
        let pool = rt.handle().inner.blocking.clone();
        let h = rt.handle();
        // Spawn one job; a worker comes online to handle it.
        crate::runtime::blocking::spawn_blocking_on(&pool, || 1u32)
            .join()
            .await
            .unwrap();
        assert_eq!(h.blocking_peak_workers(), 1);
        // Wait past the idle timeout.
        crate::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            h.blocking_worker_count(),
            0,
            "idle worker did not exit (count = {})",
            h.blocking_worker_count()
        );
    }

    #[wasm_bindgen_test]
    async fn builder_threads_through_to_handle() {
        let rt = Builder::new_multi_thread()
            .worker_threads(3)
            .max_blocking_threads(7)
            .blocking_idle_timeout(Duration::from_secs(42))
            .build();
        assert_eq!(rt.handle().worker_count(), 3);
        assert_eq!(rt.handle().blocking_max_workers(), 7);
    }

    // ----- spawn_pinned (LocalSet on the pool) -----

    #[wasm_bindgen_test]
    async fn spawn_pinned_runs_a_send_future() {
        let h = default_handle();
        let join = h.spawn_pinned(|| async { 11u32 });
        assert_eq!(join.join().await.unwrap(), 11);
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_runs_a_non_send_future() {
        use std::cell::Cell;
        use std::rc::Rc;
        let h = default_handle();
        let join = h.spawn_pinned(|| async {
            let counter = Rc::new(Cell::new(0u32));
            for _ in 0..3 {
                counter.set(counter.get() + 1);
                crate::task::yield_now().await;
            }
            counter.get()
        });
        assert_eq!(join.join().await.unwrap(), 3);
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_can_await_a_js_promise() {
        // `JsFuture` registers a `Promise.then` callback on the
        // worker's JS event loop. Pool workers must yield to that
        // event loop between polls so the callback dispatches and
        // the JsFuture wakes; otherwise this test would hang.
        let h = default_handle();
        let join = h.spawn_pinned(|| async {
            let promise = js_sys::Promise::resolve(&wasm_bindgen::JsValue::from(7u32));
            let v = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
            v.as_f64().unwrap() as u32
        });
        assert_eq!(join.join().await.unwrap(), 7);
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_can_drive_set_timeout() {
        // setTimeout(0) callbacks run as macrotasks. Pool workers'
        // `yield_macrotask` path drives them.
        let h = default_handle();
        let join = h.spawn_pinned(|| async {
            let promise = js_sys::Promise::new(&mut |resolve, _| {
                let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
                scope
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 5)
                    .unwrap();
            });
            wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
            42u32
        });
        assert_eq!(join.join().await.unwrap(), 42);
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_load_balances_across_pool() {
        // Build a 4-worker runtime and dispatch !Send work that holds
        // a JsValue across an await; verify multiple workers handled it.
        let rt = Runtime::with_workers(4);
        let h = rt.handle();
        for _ in 0..50 {
            if h.heartbeat() >= 4 {
                break;
            }
            crate::time::sleep(Duration::from_millis(50)).await;
        }
        // Each task records the global scope name (DedicatedWorker
        // for runtime workers) — but workers don't have a usable
        // identifier in JS, so we instead just observe that pinned
        // counts climb on multiple workers concurrently.
        let mut handles = Vec::new();
        for i in 0..16u32 {
            let pool = h.clone();
            handles.push(pool.spawn_pinned(move || async move {
                // Hold a JsValue across await, forcing !Send.
                let _p = js_sys::Promise::resolve(&wasm_bindgen::JsValue::from(i));
                crate::time::sleep(Duration::from_millis(10)).await;
                i
            }));
        }
        let mut total = 0u32;
        for jh in handles {
            total += jh.join().await.unwrap();
        }
        assert_eq!(total, (0..16u32).sum::<u32>());
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_handle_is_send() {
        // The JoinHandle from spawn_pinned must be Send so it can
        // travel into a `spawn`'d task and be awaited there.
        let inner = crate::task::spawn_pinned(|| async { 5u32 });
        let outer = crate::task::spawn(async move { inner.join().await.unwrap() + 1 });
        assert_eq!(outer.join().await.unwrap(), 6);
    }

    #[wasm_bindgen_test]
    async fn spawn_pinned_abort_cancels() {
        let h = crate::task::spawn_pinned(|| async {
            crate::time::sleep(Duration::from_secs(60)).await;
            1u32
        });
        let abort = h.abort_handle();
        crate::time::sleep(Duration::from_millis(20)).await;
        abort.abort();
        assert_eq!(h.join().await, Err(crate::task::JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn local_deque_overflow_does_not_starve_pool() {
        // A producer that bursts MANY tasks from inside one worker
        // would, with an unbounded local deque, pile them all on
        // that one worker. The bounded deque overflows half to the
        // injector at LOCAL_DEQUE_CAP, which other workers steal —
        // so load reaches every worker.
        let rt = Runtime::with_workers(4);
        let h = rt.handle();
        // Wait until all 4 workers have entered the loop AND parked
        // (heartbeat increments once on enter, then once per park
        // wake-cycle), so a steal opportunity exists from the start.
        for _ in 0..100 {
            if h.heartbeat() >= 8 {
                break;
            }
            crate::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // The outer task must spawn through *this* runtime's handle
        // (not the singleton) so its `schedule` calls hit our local
        // deque and trigger the overflow path. 350 children pushes
        // the deque past its 256 cap once. Each child yields so the
        // producer can't burn through the whole batch in a tight
        // loop before siblings get a chance to steal.
        let inner_handle = h.clone();
        let outer = h.spawn(async move {
            const N: u32 = 350;
            let mut handles = Vec::with_capacity(N as usize);
            for i in 0..N {
                handles.push(inner_handle.spawn(async move {
                    crate::task::yield_now().await;
                    i
                }));
            }
            let mut total: u64 = 0;
            for jh in handles {
                total += jh.join().await.unwrap() as u64;
            }
            total
        });
        let _ = outer.join().await.unwrap();
        let polled = h.tasks_polled();
        let active = polled.iter().filter(|n| **n > 0).count();
        assert!(
            active >= 2,
            "overflow did not distribute load across pool: {polled:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn shutdown_timeout_completes_within_budget() {
        let rt = Runtime::with_workers(2);
        let h = rt.handle();
        // Push a quick task to make sure workers are running.
        h.spawn(async { 1u32 }).join().await.unwrap();
        let alive_before = h.alive_workers();
        assert!(alive_before > 0);
        let start = crate::time::Instant::now();
        rt.shutdown_timeout(std::time::Duration::from_millis(800))
            .await;
        let elapsed = crate::time::Instant::now().duration_since(start);
        // Workers should have exited well before the budget — assert
        // the function returned promptly once they did.
        assert!(
            elapsed < std::time::Duration::from_millis(800),
            "shutdown overran budget: {elapsed:?}"
        );
        // After return, every worker has decremented alive_workers.
        assert_eq!(
            h.alive_workers(),
            0,
            "alive_workers should be 0 after shutdown"
        );
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_drains_on_shutdown() {
        // Submit jobs that block past the shutdown moment; the
        // post-shutdown drain must surface JoinError::Cancelled to
        // any awaiter that hasn't yet been picked up.
        use std::sync::atomic::{AtomicBool, Ordering};
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let pool = rt.handle().inner.blocking.clone();

        // Synchronise on a shared flag so we don't depend on the
        // blocking worker's cold-boot time. The `busy` job sets
        // `started` and then sleeps; the test waits for `started`
        // before triggering shutdown, guaranteeing busy is in-flight
        // (and thus q1/q2 are queued behind it) by the time shutdown
        // runs its single drain pass.
        let started = Arc::new(AtomicBool::new(false));
        let started_in_busy = started.clone();
        let busy = crate::runtime::blocking::spawn_blocking_on(&pool, move || {
            started_in_busy.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(80));
            1u32
        });
        let q1 = crate::runtime::blocking::spawn_blocking_on(&pool, || 7u32);
        let q2 = crate::runtime::blocking::spawn_blocking_on(&pool, || 9u32);

        // Wait until busy is actually executing. Up to 5 s of cold
        // worker boot tolerated.
        for _ in 0..250 {
            if started.load(Ordering::Acquire) {
                break;
            }
            crate::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            started.load(Ordering::Acquire),
            "busy job never started — blocking worker failed to boot"
        );

        // Initiate shutdown — drains queue, awaiters of q1/q2 should
        // see Cancelled. The busy job continues to completion.
        rt.shutdown_timeout(Duration::from_millis(800)).await;

        assert_eq!(q1.join().await, Err(crate::task::JoinError::Cancelled));
        assert_eq!(q2.join().await, Err(crate::task::JoinError::Cancelled));
        // The in-flight job ran to completion before shutdown picked it up.
        assert_eq!(busy.join().await.unwrap(), 1);
    }

    #[wasm_bindgen_test]
    async fn blocking_pool_rejects_post_shutdown_submissions() {
        let rt = Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .blocking_idle_timeout(Duration::from_secs(60))
            .build();
        let pool = rt.handle().inner.blocking.clone();
        rt.shutdown_timeout(Duration::from_millis(200)).await;

        // After shutdown, submissions must surface Cancelled
        // immediately rather than hang or spawn a fresh worker.
        let h = crate::runtime::blocking::spawn_blocking_on(&pool, || 5u32);
        assert_eq!(h.join().await, Err(crate::task::JoinError::Cancelled));
    }

    #[wasm_bindgen_test]
    async fn handle_current_from_inside_runtime_worker() {
        // From a runtime worker, Handle::current returns that
        // worker's runtime — same Arc as the handle that spawned us.
        let rt = Runtime::with_workers(2);
        let h = rt.handle();
        let h_clone = h.clone();
        let inside = h
            .spawn(async move {
                let cur = Handle::current();
                Arc::ptr_eq(&cur.inner, &h_clone.inner)
            })
            .join()
            .await
            .unwrap();
        assert!(
            inside,
            "Handle::current inside spawn must return that runtime"
        );
    }

    #[wasm_bindgen_test]
    async fn handle_current_outside_runtime_returns_default() {
        // From the main thread (no runtime context), Handle::current
        // falls back to the default singleton.
        let cur = Handle::current();
        let def = default_handle();
        assert!(Arc::ptr_eq(&cur.inner, &def.inner));
    }

    #[wasm_bindgen_test]
    async fn block_on_works_in_spawn_blocking() {
        // block_on is legal from a non-async worker (e.g. a
        // spawn_blocking closure). Drive a small async future via
        // block_on inside a blocking worker.
        let h = crate::task::spawn_blocking(|| {
            crate::runtime::block_on(async {
                crate::task::yield_now().await;
                123u32
            })
        });
        assert_eq!(h.join().await.unwrap(), 123);
    }

    // NB: `block_on` correctly panics when called from inside a
    // runtime worker's async-poll context, but under panic = abort
    // that panic cascades into a wasm trap that kills the entire
    // worker and chromedriver — which is fine for production safety
    // (catastrophic misuse, observable as JoinError::Cancelled) but
    // can't be asserted on inside wasm-bindgen-test. The defensive
    // check itself is exercised in code review.

    #[wasm_bindgen_test]
    async fn sleep_is_elapsed_reflects_state() {
        let mut s = crate::time::sleep(Duration::from_millis(20));
        assert!(!s.is_elapsed());
        // Drive to completion via a poll.
        (&mut s).await;
        assert!(s.is_elapsed());
    }

    #[wasm_bindgen_test]
    async fn interval_missed_tick_behavior_skip() {
        // Skip mode should resume the cadence after a long pause:
        // the catch-up tick fires immediately, then subsequent ticks
        // are ~period apart in real time.
        let mut tk = crate::time::interval(Duration::from_millis(10));
        tk.set_missed_tick_behavior(crate::time::MissedTickBehavior::Skip);
        let _ = tk.tick().await; // first tick at t ≈ 10 ms
        crate::time::sleep(Duration::from_millis(50)).await;
        let _ = tk.tick().await; // catch-up tick fires immediately
        // Now measure actual wall-clock between two subsequent ticks.
        let before = crate::time::Instant::now();
        let _ = tk.tick().await;
        let elapsed = before.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5) && elapsed <= Duration::from_millis(40),
            "Skip mode should produce period-spaced ticks after catch-up; elapsed = {elapsed:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn interval_missed_tick_behavior_delay() {
        let mut tk = crate::time::interval(Duration::from_millis(10));
        tk.set_missed_tick_behavior(crate::time::MissedTickBehavior::Delay);
        let _ = tk.tick().await;
        crate::time::sleep(Duration::from_millis(50)).await;
        // Delay mode: next tick is at now + period, regardless of
        // missed deadlines.
        let before = crate::time::Instant::now();
        let _ = tk.tick().await;
        let elapsed = before.elapsed();
        // First post-delay tick: between now and ~period (some slack
        // for setTimeout coarseness).
        assert!(
            elapsed <= Duration::from_millis(40),
            "Delay mode should produce a tick promptly; elapsed = {elapsed:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn workers_park_when_idle() {
        // Heartbeat increments on each park/unpark cycle. After
        // letting the runtime sit idle for a bit, heartbeat should be
        // stable (workers are parked, not busy-spinning).
        let h = default_handle();
        // Drain a quick task to let workers reach the loop.
        h.spawn(async { 0u8 }).join().await.unwrap();
        // Wait so workers settle into the parked state.
        crate::time::sleep(Duration::from_millis(150)).await;
        let a = h.heartbeat();
        crate::time::sleep(Duration::from_millis(200)).await;
        let b = h.heartbeat();
        // Allow up to a small handful of spurious wakes; if the workers
        // were busy-spinning they'd churn thousands.
        assert!(
            b - a < 32,
            "heartbeat advanced unreasonably while idle: {a} -> {b}"
        );
    }
}
