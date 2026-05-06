//! Main-thread receive bus.
//!
//! Every Web Worker we spawn (runtime async-pool worker or blocking-
//! pool worker) gets a single shared `onmessage` listener installed on
//! main. The listener decodes the small JS envelope `{kind, ...}` that
//! workers post back and dispatches by `kind`:
//!
//! - `wasmt_wake` carries `{slot_id, ptr}` and routes to
//!   [`super::cross::__wasmt_deliver_slot`], the cross-thread result
//!   delivery bridge.
//! - `wasmt_timer_kick` carries no payload and routes to
//!   [`super::timer::__wasmt_timer_kick`], asking main to drain
//!   expired timers and reschedule its `setTimeout`.
//!
//! Any other envelope is ignored, so unrelated `Worker.postMessage`
//! traffic does not interfere.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use super::cross::__wasmt_deliver_slot;
use super::timer::__wasmt_timer_kick;

/// One-shot callback fired exactly once when this worker's `onerror`
/// reports a fatal error (panic under `panic = "abort"`, OOM, etc.).
/// Owners use this to decrement liveness/worker-count counters whose
/// clean-exit path the dead worker never executed. `'static` so it
/// can outlive the calling stack frame inside the leaked Closure.
pub type DeathCallback = Box<dyn FnOnce() + 'static>;

/// Install the shared `onmessage` listener on `worker`, plus an
/// `onerror` that surfaces worker failures to the main-page console
/// AND — if `on_death` is `Some` — invokes the callback exactly once
/// on the first error event. Used by:
///
/// - Runtime async-pool workers, to decrement `alive_workers` so
///   [`super::Runtime::shutdown_timeout`] returns early instead of
///   waiting the full budget when a worker has trapped.
/// - Blocking-pool workers, to decrement `worker_count` so a panic
///   in a blocking job doesn't permanently inflate the pool and lock
///   up future `spawn_blocking` calls (the dead worker would
///   otherwise hold its slot forever).
///
/// Both Closures are leaked (`Closure::forget`) — they live as long
/// as the Worker. Pass `None` if no death tracking is needed.
pub fn install_listener(worker: &web_sys::Worker, on_death: Option<DeathCallback>) {
    // Wrap in Rc<Cell<Option<...>>> so the FnMut error closure can
    // consume the callback on first fire and become a no-op
    // afterward (errors can fire multiple times for the same worker).
    let death_slot: Rc<Cell<Option<DeathCallback>>> = Rc::new(Cell::new(on_death));
    let err_cb = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |ev: web_sys::ErrorEvent| {
        web_sys::console::error_2(
            &"wasmt: worker error:".into(),
            &format!("{} at {}:{}", ev.message(), ev.filename(), ev.lineno()).into(),
        );
        if let Some(cb) = death_slot.take() {
            cb();
        }
    });
    worker.set_onerror(Some(err_cb.as_ref().unchecked_ref()));
    err_cb.forget();

    // Pre-allocate the JS string keys we use to decode every
    // incoming message. `JsValue::from(&str)` allocates a fresh JS
    // string per call; doing it once at listener install saves N
    // string allocations per message under high cross-channel
    // throughput.
    let key_kind: wasm_bindgen::JsValue = "kind".into();
    let key_slot_id: wasm_bindgen::JsValue = "slot_id".into();
    let key_ptr: wasm_bindgen::JsValue = "ptr".into();
    // Move the cached keys into the closure.
    let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
        let data = ev.data();
        let Some(kind) = js_sys::Reflect::get(&data, &key_kind)
            .ok()
            .and_then(|v| v.as_string())
        else {
            return;
        };
        match kind.as_str() {
            "wasmt_wake" => {
                let slot_id = read_u32_with_key(&data, &key_slot_id);
                let ptr = read_u32_with_key(&data, &key_ptr);
                if slot_id != 0 {
                    __wasmt_deliver_slot(slot_id, ptr);
                }
            }
            "wasmt_timer_kick" => __wasmt_timer_kick(),
            _ => {}
        }
    });
    worker.set_onmessage(Some(cb.as_ref().unchecked_ref()));
    cb.forget();
}

fn read_u32_with_key(data: &wasm_bindgen::JsValue, key: &wasm_bindgen::JsValue) -> u32 {
    js_sys::Reflect::get(data, key)
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| n as u32)
        .unwrap_or(0)
}
