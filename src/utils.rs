//! Small runtime introspection helpers.
//!
//! Several `std` APIs are stubbed-out on `wasm32-unknown-unknown`:
//! `std::thread::available_parallelism()` returns `Err`, and
//! `std::thread::current().id()` is a placeholder. The helpers here
//! fill exactly those two gaps for browser code.

use std::cell::Cell;
use std::num::NonZeroUsize;

use wasm_bindgen::JsCast;
use web_sys::WorkerGlobalScope;

thread_local! {
    /// Cached result of [`is_worker_scope`]. A thread's global scope
    /// type is invariant for its lifetime, so we only need one JS
    /// reflection round-trip per thread instead of one per call.
    /// `None` = uninitialised, `Some(b)` = cached value.
    static IS_WORKER_SCOPE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// `true` iff the current thread's global scope is a `WorkerGlobalScope`
/// (dedicated worker, shared worker, or service worker).
///
/// Result is cached per thread on first call.
pub fn is_worker_scope() -> bool {
    IS_WORKER_SCOPE.with(|c| match c.get() {
        Some(b) => b,
        None => {
            let b = js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok();
            c.set(Some(b));
            b
        }
    })
}

/// Number of logical processors available to the current context.
///
/// Replaces `std::thread::available_parallelism()`, which returns
/// `Err` on `wasm32-unknown-unknown`. Reads
/// `navigator.hardwareConcurrency`, falling back to `1` if neither
/// `Window.navigator` nor `WorkerNavigator` is reachable from the
/// current scope. Always returns a value ≥ 1.
///
/// Use as a portable parallelism hint (e.g. for sizing a custom
/// worker pool). The default [`crate::Runtime`] already uses this
/// value when no explicit `worker_threads` is configured.
pub fn available_parallelism() -> NonZeroUsize {
    let n = crate::runtime::hardware_concurrency().max(1);
    // SAFETY: clamped to `>= 1` above.
    unsafe { NonZeroUsize::new_unchecked(n) }
}

/// Stable per-thread identity in the wasmt runtime.
///
/// Replaces `std::thread::current().id()`, which is a placeholder on
/// `wasm32-unknown-unknown`. Returns:
///
/// - [`ThreadId::Pool(i)`] from a runtime async-pool worker (the
///   worker's index, in `0..worker_threads`).
/// - [`ThreadId::Main`] from the main thread.
/// - [`ThreadId::Other`] from any other Web Worker (a
///   `spawn_blocking` worker, an unrelated Worker the user created,
///   etc.).
///
/// Cheap: thread-local read after the first call. Useful for
/// per-thread sharded data structures, hash partitioning, and debug
/// logging.
pub fn thread_id() -> ThreadId {
    if let Some(idx) = crate::runtime::current_pool_worker_index() {
        return ThreadId::Pool(idx);
    }
    if is_worker_scope() {
        ThreadId::Other
    } else {
        ThreadId::Main
    }
}

/// Stable per-thread identity returned by [`thread_id`]. Equality is
/// well-defined; ordering is not (the variants are categorical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadId {
    /// The main thread (window scope).
    Main,
    /// Async-pool worker `i` (in `0..worker_threads`).
    Pool(usize),
    /// A Web Worker that isn't a wasmt async-pool worker
    /// (`spawn_blocking` worker, user-spawned worker, etc.).
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn is_worker_scope_main_is_false() {
        assert!(!is_worker_scope());
    }

    #[wasm_bindgen_test]
    async fn is_worker_scope_inside_spawn_is_true() {
        let h = task::spawn(async { is_worker_scope() });
        assert!(h.join().await.unwrap());
    }

    #[wasm_bindgen_test]
    async fn is_worker_scope_inside_spawn_local_is_false() {
        let h = task::spawn_local(async { is_worker_scope() });
        assert!(!h.join().await.unwrap());
    }

    #[wasm_bindgen_test]
    async fn available_parallelism_is_at_least_one() {
        let n = available_parallelism();
        assert!(n.get() >= 1);
    }

    #[wasm_bindgen_test]
    async fn thread_id_main_is_main() {
        assert_eq!(thread_id(), ThreadId::Main);
    }

    #[wasm_bindgen_test]
    async fn thread_id_inside_pool_is_pool() {
        let h = task::spawn(async { thread_id() });
        let id = h.join().await.unwrap();
        assert!(matches!(id, ThreadId::Pool(_)), "got {id:?}");
    }

    #[wasm_bindgen_test]
    async fn thread_id_inside_blocking_is_other() {
        let h = task::spawn_blocking(thread_id);
        assert_eq!(h.join().await.unwrap(), ThreadId::Other);
    }

    #[wasm_bindgen_test]
    async fn thread_id_pool_indices_distribute() {
        // Across many tasks, more than one worker index appears.
        use crate::runtime::Runtime;
        let rt = Runtime::with_workers(4);
        let h = rt.handle();
        // Wait for workers to boot.
        for _ in 0..50 {
            if h.heartbeat() >= 4 {
                break;
            }
            crate::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut handles = Vec::new();
        for _ in 0..32 {
            handles.push(h.spawn(async {
                // Tiny CPU spin so workers actually contend.
                let mut acc = 0u32;
                for i in 0..1_000 {
                    acc = acc.wrapping_add(i);
                }
                let id = thread_id();
                std::hint::black_box(acc);
                id
            }));
        }
        let mut seen = std::collections::HashSet::new();
        for jh in handles {
            if let ThreadId::Pool(i) = jh.join().await.unwrap() {
                seen.insert(i);
            }
        }
        assert!(
            seen.len() >= 2,
            "expected work to span ≥ 2 workers; saw {seen:?}"
        );
    }
}
