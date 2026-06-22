//! Run work on the browser **main thread** from anywhere.
//!
//! Some Web APIs exist only on the main thread and are simply absent in
//! a worker's global scope — `window`, the DOM (`document`),
//! `localStorage`/`sessionStorage`, `history`, `alert`/`prompt`, parts
//! of `navigator`, etc. A pool or `spawn_blocking` worker that needs one
//! of those has to hand the work back to main. [`spawn_on_main`] is the
//! symmetric counterpart to [`crate::spawn_pinned`]: it pins a future to
//! the main thread and returns a [`JoinHandle`] you can await from any
//! thread.
//!
//! Mechanism: the constructor closure is pushed onto a shared lock-free
//! queue and the main thread is poked with a `wasmt_main_job`
//! `postMessage` (main's per-worker [`super::main_bus`] listener drains
//! the queue and runs each job via `wasm_bindgen_futures::spawn_local`,
//! so the future executes on main's event loop). The result travels back
//! through a `oneshot` whose waker is safe to fire main → worker
//! (runtime/`wasm_bindgen_futures` wakers re-enqueue via shared memory +
//! `Atomics.notify`, which is legal on every thread). When called from
//! main itself, the job runs inline with no `postMessage` hop.

use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_queue::SegQueue;
use futures::future::{AbortHandle as RawAbortHandle, Abortable};
use wasm_bindgen::JsCast;

use crate::task::{AbortHandle, Id, JoinHandle, Recv, WithId};

/// A `!Send` constructor ferried from any thread to main, run exactly
/// once there. Same one-shot-total-move soundness argument as
/// [`super::PinnedJob`] / [`super::blocking::BlockingJob`].
struct MainJob(Box<dyn FnOnce() + 'static>);

// SAFETY: pushed by one thread, popped and consumed exactly once on
// main; never observed concurrently.
unsafe impl Send for MainJob {}

impl MainJob {
    fn run(self) {
        (self.0)();
    }
}

static MAIN_JOBS: OnceLock<SegQueue<MainJob>> = OnceLock::new();

fn jobs() -> &'static SegQueue<MainJob> {
    MAIN_JOBS.get_or_init(SegQueue::new)
}

/// Drain and run every queued main-thread job. Runs **on main** — either
/// invoked by [`super::main_bus`] on a `wasmt_main_job` message, or
/// inline when [`spawn_on_main`] is called from the main thread.
pub(crate) fn drain_main_jobs() {
    let q = jobs();
    while let Some(job) = q.pop() {
        job.run();
    }
}

// Per-worker cached `postMessage` payload for the main-job kick, mirroring
// `timer::kick` — `JsValue::from(&str)` allocates a fresh JS string each
// call, so we build the envelope once per worker and reuse it.
struct KickState {
    scope: web_sys::DedicatedWorkerGlobalScope,
    payload: js_sys::Object,
}

thread_local! {
    static KICK_STATE: std::cell::OnceCell<Option<KickState>> = const { std::cell::OnceCell::new() };
}

fn kick_main() {
    KICK_STATE.with(|cell| {
        let st = cell.get_or_init(|| {
            let scope = js_sys::global()
                .dyn_into::<web_sys::DedicatedWorkerGlobalScope>()
                .ok()?;
            let payload = js_sys::Object::new();
            js_sys::Reflect::set(&payload, &"kind".into(), &"wasmt_main_job".into()).unwrap();
            Some(KickState { scope, payload })
        });
        match st {
            Some(state) => {
                let _ = state.scope.post_message(&state.payload);
            }
            // Not in a DedicatedWorker scope (e.g. an exotic host). The
            // job is queued; it will run when main next drains. Best
            // effort — fall back to a direct drain attempt.
            None => drain_main_jobs(),
        }
    });
}

/// Spawn a future that runs on the **main thread**, returning a
/// [`JoinHandle`] awaitable from any thread.
///
/// Use this for main-thread-only Web APIs (DOM, `window`,
/// `localStorage`, `history`, …) from a pool or `spawn_blocking` worker.
/// The constructor `f` runs *on main*, so build any realm-bound
/// `JsValue` / `web-sys` handles **inside** `f` — do not capture them
/// from the calling worker's realm (they belong to a different JS
/// realm). The output `T` must be `Send` so it can travel back to the
/// caller; return plain Rust data extracted from the JS work, not
/// `JsValue`s.
///
/// When called from the main thread the future is simply driven on main
/// with no cross-thread hop.
pub fn spawn_on_main<F, Fut, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (raw_abort, reg) = RawAbortHandle::new_pair();
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    let id = Id::next();

    // `oneshot` is safe in either direction here: the receiver is always
    // a runtime task or a `wasm_bindgen_futures` task, whose wakers
    // re-enqueue via shared memory + `Atomics.notify` (legal on main).
    let (tx, rx) = futures::channel::oneshot::channel::<T>();

    let constructor = MainJob(Box::new(move || {
        let fut = f();
        let abortable = Abortable::new(fut, reg);
        let wrapped = WithId {
            id,
            inner: async move {
                if let Ok(out) = abortable.await {
                    let _ = tx.send(out);
                }
                done.store(true, Ordering::Release);
            },
        };
        // Runs on main: drives the (possibly `!Send`) future on main's
        // event loop, where main-only Web APIs are available.
        wasm_bindgen_futures::spawn_local(wrapped);
    }));

    jobs().push(constructor);
    if crate::utils::is_worker_scope() {
        kick_main();
    } else {
        // Already on main — run the constructor now (it just schedules
        // the future via spawn_local; it does not block).
        drain_main_jobs();
    }

    JoinHandle::new(
        Recv::Local(rx),
        AbortHandle::from_raw(raw_abort, finished.clone(), id),
        finished,
        id,
    )
}
