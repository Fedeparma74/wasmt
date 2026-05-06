//! Worker-side entry point. Called by `worker.js` after `init(module,
//! memory)` resolves on a freshly-booted Web Worker. Drives the
//! executor loop until the runtime is shut down. JS handles
//! `__wbindgen_thread_destroy` and `close()` after this function's
//! Promise resolves.

use wasm_bindgen::prelude::wasm_bindgen;

use super::{WorkerBootstrap, worker_loop};

/// Runtime-worker entry point. The pointer is a `Box<WorkerBootstrap>`
/// produced by `Runtime::new` on main; we reclaim it, then hand off to
/// the executor loop. The fn is async because the loop yields to the
/// worker's JS event loop whenever pinned tasks are alive (so JS
/// `Promise.then` callbacks dispatch).
#[wasm_bindgen]
pub async fn runtime_worker_main(boot_ptr: u32) {
    console_error_panic_hook::set_once();
    let boot = unsafe { Box::from_raw(boot_ptr as usize as *mut WorkerBootstrap) };
    worker_loop(*boot).await;
}
