//! Dynamic blocking-task pool.
//!
//! Each runtime owns a [`BlockingPool`]: a lazy, capped, idle-shrinking
//! set of long-lived Web Workers that serve [`spawn_blocking`] calls.
//!
//! - **Lazy growth**: a new worker is spawned only when there's a job
//!   pending and no parked worker can take it.
//! - **Cap**: at most `max_workers` workers exist at once. Beyond that,
//!   new jobs queue up until a worker becomes free.
//! - **Idle shrink**: a worker that has been idle for
//!   `idle_timeout` exits cleanly. The next `spawn_blocking` will spawn
//!   a fresh worker if the pool needs it again.
//!
//! Workers park on `Atomics.wait` with a finite timeout so the
//! idle-exit branch wakes correctly. Jobs are dispatched through a
//! lock-free [`SegQueue`]; result delivery still goes through
//! [`crate::runtime::cross`] (worker → main) or a plain `oneshot`
//! (worker → worker), as in the async pool.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_queue::SegQueue;
use futures::future::AbortHandle as RawAbortHandle;
use wasm_bindgen::prelude::wasm_bindgen;

use crate::runtime::cross;
use crate::runtime::main_bus;
use crate::task::{AbortHandle, JoinHandle, Recv};
use crate::utils::is_worker_scope;

/// A unit of blocking work. The closure already owns the result-delivery
/// channel and the `finished` atomic; running it consumes both.
///
/// `BlockingJob` wraps a `!Send` `Box<dyn FnOnce()>` and asserts `Send`
/// manually. The wrapper is **only** ferried from the spawning thread
/// to a single blocking-pool worker via the lock-free job queue; the
/// closure is consumed exactly once on the destination worker, with
/// no aliasing at any point. Soundness justification mirrors
/// [`crate::runtime::PinnedJob`] and [`crate::runtime::local::LocalTask`].
///
/// **Caveat**: avoid capturing realm-bound state (`JsValue`, `web-sys`
/// handles) directly — the worker runs in a different JS realm and
/// such captures would be invalid when the closure runs.
pub(crate) struct BlockingJob(Box<dyn FnOnce() + 'static>);

// SAFETY: see the type doc — one-shot total move via `SegQueue<BlockingJob>`,
// consumed exactly once on the destination worker.
unsafe impl Send for BlockingJob {}

impl BlockingJob {
    pub(crate) fn new(f: impl FnOnce() + 'static) -> Self {
        BlockingJob(Box::new(f))
    }

    pub(crate) fn run(self) {
        (self.0)();
    }
}

const PARK_EMPTY: i32 = 0;
const PARK_NOTIFIED: i32 = 1;

#[wasm_bindgen(module = "/workerSpawner.js")]
extern "C" {
    #[wasm_bindgen(js_name = spawnBlockingPoolWorker, catch)]
    fn spawn_blocking_pool_worker(
        module: &wasm_bindgen::JsValue,
        memory: &wasm_bindgen::JsValue,
        pool_ptr: u32,
    ) -> Result<web_sys::Worker, wasm_bindgen::JsValue>;
}

pub(crate) struct BlockingPoolInner {
    queue: SegQueue<BlockingJob>,
    parking: AtomicI32,
    parked_count: AtomicUsize,
    /// Currently-alive workers, including those mid-job.
    worker_count: AtomicUsize,
    /// Cap on `worker_count`. New jobs above the cap queue.
    max_workers: usize,
    /// Idle wait timeout in nanoseconds; a worker that times out
    /// cleanly exits. `i64::MAX` disables idle exit.
    idle_timeout_ns: i64,
    shutdown: AtomicBool,
    /// Diagnostic: total jobs run by the pool.
    jobs_run: AtomicUsize,
    /// Diagnostic: peak `worker_count` reached.
    peak_workers: AtomicUsize,
}

/// Cheap, cloneable handle to the blocking pool. Held by the runtime;
/// shared with workers via shared memory.
#[derive(Clone)]
pub(crate) struct BlockingPool {
    inner: Arc<BlockingPoolInner>,
}

impl BlockingPool {
    pub fn new(max_workers: usize, idle_timeout: Duration) -> Self {
        let max = max_workers.max(1);
        let timeout_ns: i64 = idle_timeout.as_nanos().try_into().unwrap_or(i64::MAX);
        BlockingPool {
            inner: Arc::new(BlockingPoolInner {
                queue: SegQueue::new(),
                parking: AtomicI32::new(PARK_EMPTY),
                parked_count: AtomicUsize::new(0),
                worker_count: AtomicUsize::new(0),
                max_workers: max,
                idle_timeout_ns: timeout_ns,
                shutdown: AtomicBool::new(false),
                jobs_run: AtomicUsize::new(0),
                peak_workers: AtomicUsize::new(0),
            }),
        }
    }

    /// Submit a job. Wakes a parked worker if any; otherwise spawns a
    /// new worker if we are below the cap. (If already at the cap,
    /// the job sits in the queue until a worker becomes available.)
    ///
    /// If the pool is already shutting down, the job is dropped on
    /// the floor; its result-delivery channel is closed when the
    /// closure drops, so the awaiting [`JoinHandle`] resolves to
    /// [`crate::task::JoinError::Cancelled`].
    pub fn submit(&self, job: BlockingJob) {
        // Push first, then re-check shutdown. If `signal_shutdown`
        // races with us, this ordering guarantees we never strand a
        // job: either signal_shutdown's drain runs after our push
        // (and it pops our job), or our re-check sees `shutdown`
        // and we drain ourselves. Without this re-check, a producer
        // observing `shutdown == false` and then pushing AFTER
        // `signal_shutdown`'s single drain pass would leave the job
        // queued forever (workers exit at top-of-loop without
        // draining), and the awaiter would hang.
        self.inner.queue.push(job);
        if self.inner.shutdown.load(Ordering::Acquire) {
            // Drain anything we (or other racing producers) just
            // pushed so awaiters see Cancelled instead of hanging.
            // Pop is concurrent-safe; over-draining is benign — all
            // remaining jobs were destined for cancellation anyway.
            while self.inner.queue.pop().is_some() {}
            return;
        }
        if self.notify_one() {
            return;
        }
        self.maybe_spawn_worker();
    }

    /// Wake one parked worker, returning whether anyone was waiting.
    fn notify_one(&self) -> bool {
        if self.inner.parked_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.inner.parking.store(PARK_NOTIFIED, Ordering::Release);
        // SAFETY: parking is in shared linear memory.
        unsafe {
            core::arch::wasm32::memory_atomic_notify(
                &self.inner.parking as *const AtomicI32 as *mut i32,
                1,
            );
        }
        true
    }

    /// Wake everybody (used on shutdown).
    fn notify_all(&self) {
        self.inner.parking.store(PARK_NOTIFIED, Ordering::Release);
        unsafe {
            core::arch::wasm32::memory_atomic_notify(
                &self.inner.parking as *const AtomicI32 as *mut i32,
                u32::MAX,
            );
        }
    }

    /// Try to atomically reserve a worker slot and spawn one. No-op if
    /// already at cap or after shutdown.
    fn maybe_spawn_worker(&self) {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut current = self.inner.worker_count.load(Ordering::Acquire);
        loop {
            if current >= self.inner.max_workers {
                return;
            }
            match self.inner.worker_count.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        // We reserved a slot. Update peak.
        let after = current + 1;
        let mut peak = self.inner.peak_workers.load(Ordering::Relaxed);
        while peak < after {
            match self.inner.peak_workers.compare_exchange(
                peak,
                after,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => peak = seen,
            }
        }

        let module = wasm_bindgen::module();
        let memory = wasm_bindgen::memory();
        let raw = Arc::into_raw(self.inner.clone()) as u32;
        match spawn_blocking_pool_worker(&module, &memory, raw) {
            Ok(worker) => {
                // From main, install the wake-bus listener so workers
                // running on this new pool worker can post results back
                // to main. From inside another worker, skip — the
                // result path is oneshot (no postMessage needed).
                if !is_worker_scope() {
                    // On blocking-worker death (e.g. a panicking job
                    // under `panic = "abort"`): saturating-decrement
                    // `worker_count` so the pool can spawn a
                    // replacement instead of permanently holding the
                    // dead worker's slot — otherwise enough panics
                    // would saturate the pool at `max_workers` and
                    // hang every subsequent `spawn_blocking`.
                    let inner = self.inner.clone();
                    main_bus::install_listener(
                        &worker,
                        Some(Box::new(move || {
                            let counter = &inner.worker_count;
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
                }
            }
            Err(err) => {
                // Reclaim the leaked Arc; spawn failed.
                let _ = unsafe { Arc::from_raw(raw as usize as *const BlockingPoolInner) };
                self.inner.worker_count.fetch_sub(1, Ordering::AcqRel);
                web_sys::console::error_1(&"wasmt: failed to spawn blocking-pool worker".into());
                panic!("wasmt::BlockingPool: failed to spawn worker: {:?}", err);
            }
        }
    }

    pub fn signal_shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        // Drop pending jobs so their result senders close and any
        // awaiting JoinHandles resolve to JoinError::Cancelled instead
        // of hanging forever waiting on a worker that won't pick them
        // up. Workers that are mid-job continue and exit at the next
        // top-of-loop check.
        while self.inner.queue.pop().is_some() {}
        self.notify_all();
    }

    pub fn worker_count(&self) -> usize {
        self.inner.worker_count.load(Ordering::Acquire)
    }

    pub fn peak_workers(&self) -> usize {
        self.inner.peak_workers.load(Ordering::Relaxed)
    }

    pub fn jobs_run(&self) -> usize {
        self.inner.jobs_run.load(Ordering::Relaxed)
    }

    pub fn max_workers(&self) -> usize {
        self.inner.max_workers
    }
}

/// Worker entry point. Loops pulling jobs from the queue; exits on
/// idle timeout or runtime shutdown. Decrements `worker_count`
/// exactly once on the way out — at the precise moment the worker
/// gives up its slot.
#[wasm_bindgen]
pub fn blocking_pool_main(pool_ptr: u32) {
    console_error_panic_hook::set_once();
    // SAFETY: pool_ptr was created via Arc::into_raw on the spawning
    // thread. We balance the refcount here — the Arc drops at scope
    // exit when the worker is shutting down.
    let inner = unsafe { Arc::from_raw(pool_ptr as usize as *const BlockingPoolInner) };
    blocking_worker_loop(&inner);
}

fn blocking_worker_loop(inner: &Arc<BlockingPoolInner>) {
    loop {
        if inner.shutdown.load(Ordering::Acquire) {
            inner.worker_count.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        if let Some(job) = inner.queue.pop() {
            // Run the job. If it panics, the wasm instance aborts and
            // the result-channel sender is dropped — the awaiter sees
            // `JoinError::Cancelled`, matching the documented contract.
            //
            // `job.run()` consumes the `BlockingJob` wrapper here: this
            // is the one and only consumption site for jobs popped from
            // the cross-thread queue, satisfying the `unsafe impl Send`
            // contract on the wrapper.
            job.run();
            inner.jobs_run.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Park with a finite timeout so we can self-exit when idle.
        let prev = inner.parking.swap(PARK_EMPTY, Ordering::AcqRel);
        if prev == PARK_NOTIFIED {
            continue;
        }

        inner.parked_count.fetch_add(1, Ordering::AcqRel);
        // Re-check the queue after announcing parked state.
        if !inner.queue.is_empty() || inner.shutdown.load(Ordering::Acquire) {
            inner.parked_count.fetch_sub(1, Ordering::AcqRel);
            continue;
        }

        let result = unsafe {
            core::arch::wasm32::memory_atomic_wait32(
                &inner.parking as *const AtomicI32 as *mut i32,
                PARK_EMPTY,
                inner.idle_timeout_ns,
            )
        };
        inner.parked_count.fetch_sub(1, Ordering::AcqRel);

        // wait32 result: 0 = OK (notified), 1 = NOT_EQUAL, 2 = TIMED_OUT.
        if result != 2 || inner.shutdown.load(Ordering::Acquire) {
            // Notified or shutdown — loop back; the top-of-loop
            // shutdown branch handles the latter.
            continue;
        }

        // Idle-exit candidate. Tentatively give up our slot so any
        // racing producer that decides to spawn a replacement sees
        // the lower `worker_count` (which closes the
        //   "producer pushes after our parked_count.fetch_sub but
        //    before we exit, sees parked_count==0 and
        //    worker_count==cap, neither notifies nor spawns,
        //    job stranded"
        // race).
        inner.worker_count.fetch_sub(1, Ordering::AcqRel);
        if !inner.queue.is_empty() {
            // A producer raced; reclaim the slot and run the job.
            inner.worker_count.fetch_add(1, Ordering::AcqRel);
            continue;
        }
        return;
    }
}

/// Public entry: enqueue a blocking closure on the runtime's pool.
///
/// `F` is **not** required to be `Send`. The closure is ferried to a
/// blocking-pool worker via [`BlockingJob`], a wrapper that asserts
/// `Send` manually because the move is one-shot total (push then pop,
/// consumed exactly once on the destination worker, no aliasing). The
/// output `T` still needs to be `Send` because it travels back to the
/// caller through a cross-thread `oneshot` / `cross::channel`.
///
/// **Caveat**: realm-bound captures (`JsValue`, `web-sys`) are valid
/// only on the realm where they were created. The blocking worker
/// runs in a different realm, so closures should not capture such
/// types directly — construct them inside `f` instead, or use plain
/// Rust types in the captures.
pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + 'static,
    T: Send + 'static,
{
    let pool = crate::runtime::default_handle().blocking().clone();
    spawn_blocking_on(&pool, f)
}

pub(crate) fn spawn_blocking_on<F, T>(pool: &BlockingPool, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + 'static,
    T: Send + 'static,
{
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    // AbortHandle is wired but disconnected — blocking can't be aborted.
    let (raw_abort, _reg) = RawAbortHandle::new_pair();
    let id = crate::task::Id::next();

    enum Tx<T> {
        Cross(cross::Sender<T>),
        Local(futures::channel::oneshot::Sender<T>),
    }
    let (tx, recv) = if is_worker_scope() {
        let (tx, rx) = futures::channel::oneshot::channel::<T>();
        (Tx::Local(tx), Recv::Local(rx))
    } else {
        let (tx, rx) = cross::channel::<T>();
        (Tx::Cross(tx), Recv::Cross(rx))
    };

    let job = BlockingJob::new(move || {
        // Establish the task id for the duration of the closure so
        // `wasmt::task::id()` works inside `spawn_blocking`.
        let result = crate::task::with_task_id(id, f);
        match tx {
            Tx::Cross(tx) => tx.send(result),
            Tx::Local(tx) => {
                let _ = tx.send(result);
            }
        }
        done.store(true, Ordering::Release);
    });
    pool.submit(job);

    JoinHandle::new(
        recv,
        AbortHandle::from_raw(raw_abort, finished.clone(), id),
        finished,
        id,
    )
}
