//! Adversarial probes for scheduler fairness / starvation / main-thread
//! Atomics.wait soundness.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use wasm_bindgen_test::*;
wasm_bindgen_test_configure!(run_in_browser);

// REGRESSION: this main-consumer/worker-producer contention pattern
// trapped ("Atomics.wait cannot be called in this context") when
// wasmt::sync re-exported tokio::sync, because tokio's internal
// std::sync::Mutex parks via Atomics.wait (illegal on main). The native
// wasmt::sync primitives are spinlock/lock-free internally, so this runs.
#[wasm_bindgen_test]
async fn mpsc_from_main_under_contention() {
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<u32>(1);
    let producer = wasmt::spawn(async move {
        for i in 0..2000u32 {
            tx.send(i).await.unwrap();
        }
    });
    let mut got = 0u32;
    while let Some(_v) = rx.recv().await {
        got += 1;
    }
    producer.join().await.unwrap();
    assert_eq!(got, 2000);
}

// Same root cause via wasmt Mutex shared main <-> worker.
#[wasm_bindgen_test]
async fn mutex_from_main_under_contention() {
    let m = Arc::new(wasmt::sync::Mutex::new(0u64));
    let m2 = m.clone();
    let worker = wasmt::spawn(async move {
        for _ in 0..5000u32 {
            let mut g = m2.lock().await;
            *g += 1;
        }
    });
    for _ in 0..5000u32 {
        let mut g = m.lock().await;
        *g += 1;
    }
    worker.join().await.unwrap();
    assert_eq!(*m.lock().await, 10_000);
}

// FAIRNESS: an indefinitely self-waking task monopolizes its worker's
// LIFO slot. A sibling sitting in that worker's LOCAL deque (single
// worker => no stealer) is never polled => permanent starvation.
#[wasm_bindgen_test]
async fn infinite_self_yield_starves_local_sibling_single_worker() {
    let rt = wasmt::Runtime::with_workers(1);
    let h = rt.handle();
    for _ in 0..50 {
        if h.heartbeat() >= 1 {
            break;
        }
        wasmt::time::sleep(Duration::from_millis(20)).await;
    }
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    let inner_h = h.clone();
    let _outer = h.spawn(async move {
        let _sib = inner_h.spawn(async move {
            ran2.store(true, Ordering::Release);
        });
        // Self-wake forever; never transition to awaiting anything.
        loop {
            wasmt::task::yield_now().await;
        }
    });
    // Give the sibling a generous window to run.
    for _ in 0..50 {
        if ran.load(Ordering::Acquire) {
            break;
        }
        wasmt::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ran.load(Ordering::Acquire),
        "sibling in local deque was starved by self-yielding monopolizer"
    );
}

// Spawn storm, detach immediately (drop handles), verify side effects run.
#[wasm_bindgen_test]
async fn spawn_storm_detached_all_run() {
    let counter = Arc::new(AtomicU32::new(0));
    for _ in 0..500u32 {
        let c = counter.clone();
        drop(wasmt::spawn(async move {
            wasmt::task::yield_now().await;
            c.fetch_add(1, Ordering::Release);
        }));
    }
    for _ in 0..200 {
        if counter.load(Ordering::Acquire) == 500 {
            break;
        }
        wasmt::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        counter.load(Ordering::Acquire),
        500,
        "not all detached tasks ran"
    );
}
