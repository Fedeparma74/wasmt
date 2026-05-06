//! Cross-thread result delivery.
//!
//! `wasm_bindgen_futures` `Waker`s hold realm-bound `JsValue`s, so a
//! worker thread cannot legally call `.wake()` on a main-thread waker
//! (the wake function would call `Promise.resolve().then(...)` on
//! worker scope, dispatching the microtask in the wrong realm — and
//! quite possibly touching JS handles that don't exist in the worker's
//! externref table).
//!
//! This module supplies the bridge: workers `postMessage` a small
//! `{slot_id, ptr}` payload back to main, main's per-worker
//! `onmessage` listener calls [`__wasmt_deliver_slot`], which moves
//! the boxed result into the slot and wakes the registered (main-
//! thread) waker.
//!
//! All slot bookkeeping lives in a `thread_local!` on the main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::wasm_bindgen;

/// Delivery callback: receives the raw pointer the worker sent and is
/// responsible for reclaiming the `Box`, storing the value into the
/// slot, and waking the receiver's waker. One-shot.
type SlotCallback = Box<dyn FnOnce(u32)>;

thread_local! {
    static SLOTS: RefCell<HashMap<u32, SlotCallback>> = RefCell::new(HashMap::new());
    static NEXT_SLOT_ID: Cell<u32> = const { Cell::new(1) };
}

fn register_slot(cb: SlotCallback) -> u32 {
    SLOTS.with(|s| {
        let mut slots = s.borrow_mut();
        // Probe forward until we find an unused id. The u32 counter
        // wraps every ~4B registrations; without the collision check,
        // wrap-around on a long-running app could overwrite a slot
        // that still hadn't been delivered, leaking the old box and
        // hanging the awaiter. The collision case is astronomically
        // rare in practice but the probe is cheap.
        loop {
            let id = NEXT_SLOT_ID.with(|n| {
                let id = n.get();
                n.set(id.wrapping_add(1).max(1));
                id
            });
            if let std::collections::hash_map::Entry::Vacant(e) = slots.entry(id) {
                e.insert(cb);
                return id;
            }
        }
    })
}

/// JS-callable bridge. Main's per-worker `onmessage` handler calls
/// this with the `{slot_id, ptr}` payload the worker posted.
///
/// The slot remains registered until the sender side delivers (with
/// either a value or a cancellation `ptr=0`). The receiver's `Drop`
/// does not unregister: a value already in flight when the receiver
/// drops would otherwise leak. The callback is still safe to fire
/// after receiver drop because it captures an `Arc<Inner<T>>`; if
/// nobody is awaiting, it just reclaims the box and lets `Inner`
/// drop.
#[wasm_bindgen]
pub fn __wasmt_deliver_slot(slot_id: u32, ptr: u32) {
    let cb = SLOTS.with(|s| s.borrow_mut().remove(&slot_id));
    if let Some(cb) = cb {
        cb(ptr);
    } else if ptr != 0 {
        // Should be unreachable in correct usage — log loudly.
        web_sys::console::error_1(
            &format!(
                "wasmt: deliver_slot for unknown id {slot_id} with non-zero \
                 ptr; this indicates a runtime bug"
            )
            .into(),
        );
    }
}

// Per-thread cache of the sticky payload object + interned JS string
// keys / discriminator. `JsValue::from(&str)` allocates a fresh JS
// string every call; for hot paths (every cross-thread send) we
// allocate the keys once per worker and reuse forever.
//
// `payload` is mutated in place across calls. `postMessage` does a
// structured-clone eagerly, so the queued message is independent of
// any subsequent mutation we make for the next send.
struct WakePostState {
    scope: web_sys::DedicatedWorkerGlobalScope,
    payload: js_sys::Object,
    /// "kind" key — initialised on the payload once at init time,
    /// never mutated again. Kept around for documentation; not read
    /// after init.
    _key_kind: wasm_bindgen::JsValue,
    key_slot_id: wasm_bindgen::JsValue,
    key_ptr: wasm_bindgen::JsValue,
    /// "wasmt_wake" value — set on the payload once at init time.
    _val_wasmt_wake: wasm_bindgen::JsValue,
}

thread_local! {
    static WAKE_POST_STATE: std::cell::OnceCell<WakePostState> =
        const { std::cell::OnceCell::new() };
}

fn post_wake(slot_id: u32, ptr: u32) {
    WAKE_POST_STATE.with(|cell| {
        let st = cell.get_or_init(|| {
            let scope = js_sys::global()
                .dyn_into::<web_sys::DedicatedWorkerGlobalScope>()
                .expect("post_wake must be called from a DedicatedWorkerGlobalScope");
            let payload = js_sys::Object::new();
            let key_kind: wasm_bindgen::JsValue = "kind".into();
            let val_wasmt_wake: wasm_bindgen::JsValue = "wasmt_wake".into();
            // The "kind" field never changes for this state — set it
            // once at init and skip on every send.
            js_sys::Reflect::set(&payload, &key_kind, &val_wasmt_wake).unwrap();
            WakePostState {
                scope,
                payload,
                _key_kind: key_kind,
                key_slot_id: "slot_id".into(),
                key_ptr: "ptr".into(),
                _val_wasmt_wake: val_wasmt_wake,
            }
        });
        // Mutate slot_id / ptr in the sticky payload. Leave "kind"
        // alone — initialised once at first call.
        js_sys::Reflect::set(&st.payload, &st.key_slot_id, &(slot_id as f64).into()).unwrap();
        js_sys::Reflect::set(&st.payload, &st.key_ptr, &(ptr as f64).into()).unwrap();
        st.scope
            .post_message(&st.payload)
            .expect("post_message failed");
    });
}

struct Inner<T> {
    completed: AtomicBool,
    /// `true` when the sender has been consumed (via `send`) — the
    /// receiver should expect a value. If `false` and `completed`,
    /// it was a cancellation drop and there is no value.
    sent: AtomicBool,
    value: Mutex<Option<T>>,
    /// Receiver's waker. `AtomicWaker` is lock-free — saves a Mutex
    /// lock on every `Receiver::poll` vs `Mutex<Option<Waker>>`.
    waker: AtomicWaker,
}

/// Send side. `Send` so it can travel into a worker (it always is
/// when `T: Send`). Holds an Arc to the slot's inner so the receiver
/// stays alive as long as the sender exists.
pub struct Sender<T> {
    inner: Arc<Inner<T>>,
    slot_id: u32,
}

/// Receive side. Stays on main.
pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T: Send + 'static> Sender<T> {
    /// Send `value` to main. Must be called from a worker scope.
    pub fn send(self, value: T) {
        // Mark as sent so the Drop impl below knows not to fire a
        // cancellation, then post the value across.
        self.inner.sent.store(true, Ordering::Release);
        let ptr = Box::into_raw(Box::new(value)) as usize as u32;
        post_wake(self.slot_id, ptr);
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // If `send` was already called, we set `sent` and there's
        // nothing more to do (the value is in flight). Otherwise the
        // sender was dropped without sending — signal cancellation so
        // the receiver doesn't block forever.
        if self.inner.sent.load(Ordering::Acquire) {
            return;
        }
        // From a worker scope, post a "ptr=0" wake; main's delivery
        // callback treats it as a cancellation. From main itself
        // (rare — the Sender is meant to travel into workers), call
        // the delivery directly.
        //
        // `is_worker_scope()` is thread-local-cached so the scope
        // check is a single load after the first call, vs the JS
        // reflection (`dyn_ref`) we'd otherwise do every drop.
        if crate::utils::is_worker_scope() {
            post_wake(self.slot_id, 0);
        } else {
            __wasmt_deliver_slot(self.slot_id, 0);
        }
    }
}

impl<T: Send + 'static> Future for Receiver<T> {
    type Output = Result<T, Cancelled>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.inner.completed.load(Ordering::Acquire) {
            return match self.inner.value.lock().unwrap().take() {
                Some(v) => Poll::Ready(Ok(v)),
                None => Poll::Ready(Err(Cancelled)),
            };
        }
        // Register-then-recheck pattern to avoid the lost-wakeup
        // race with the cb that fires `completed.store + waker.wake`.
        self.inner.waker.register(cx.waker());
        if self.inner.completed.load(Ordering::Acquire) {
            return match self.inner.value.lock().unwrap().take() {
                Some(v) => Poll::Ready(Ok(v)),
                None => Poll::Ready(Err(Cancelled)),
            };
        }
        Poll::Pending
    }
}

// `Receiver` does not unregister its slot on drop: a value already
// in flight (worker called `Sender::send` but main hasn't dispatched
// the `wasmt_wake` message yet) would leak its box. Leaving the
// slot registered lets the inevitable `__wasmt_deliver_slot` call
// reclaim it. The callback captures an `Arc<Inner<T>>`, so the
// inner stays alive long enough to receive the value; nothing reads
// it after the receiver is gone, then everything drops together.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Cancelled;

#[doc(hidden)]
pub fn slots_pending() -> usize {
    SLOTS.with(|s| s.borrow().len())
}

/// Create a one-shot cross-thread channel.
///
/// **Must be called from the main thread.**
pub fn channel<T: Send + 'static>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner::<T> {
        completed: AtomicBool::new(false),
        sent: AtomicBool::new(false),
        value: Mutex::new(None),
        waker: AtomicWaker::new(),
    });
    let cb_inner = inner.clone();
    let slot_id = register_slot(Box::new(move |ptr: u32| {
        if ptr != 0 {
            // SAFETY: ptr was created by `Box::into_raw(Box::<T>::new(...))`
            // in `Sender::send` and is owned by us now.
            let value: Box<T> = unsafe { Box::from_raw(ptr as usize as *mut T) };
            *cb_inner.value.lock().unwrap() = Some(*value);
        }
        // ptr == 0 indicates Sender drop without send — cancellation.
        cb_inner.completed.store(true, Ordering::Release);
        cb_inner.waker.wake();
    }));
    (
        Sender {
            inner: inner.clone(),
            slot_id,
        },
        Receiver { inner },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn channel_registers_and_drains_on_sender_drop() {
        let before = slots_pending();
        let (tx, rx) = channel::<u32>();
        assert_eq!(slots_pending(), before + 1);
        // Dropping the receiver alone does not unregister: a value
        // could be in flight, and removing the slot now would leak
        // the box. The slot lives until the sender finalises.
        drop(rx);
        assert_eq!(slots_pending(), before + 1);
        // Sender drop posts a cancellation; main delivers it, the
        // callback runs and the slot drains.
        drop(tx);
        // Cancellation goes via `__wasmt_deliver_slot` directly when
        // the sender drops on the main thread.
        assert_eq!(slots_pending(), before);
    }

    #[wasm_bindgen_test]
    async fn channel_delivers_via_pool() {
        // End-to-end: spawn a worker task that sends through the
        // cross channel and verify the main-thread receiver wakes.
        let h = crate::runtime::default_handle();
        let join = h.spawn(async { 12345u32 });
        assert_eq!(join.join().await.unwrap(), 12345);
    }

    #[wasm_bindgen_test]
    fn slot_id_increments() {
        let (t1, _r1) = channel::<()>();
        let id1 = t1.slot_id;
        let (t2, _r2) = channel::<()>();
        let id2 = t2.slot_id;
        assert!(id2 > id1, "slot ids should increment: {id1} {id2}");
    }
}
