//! Tests for `spawn_on_main` — pinning work to the browser main thread
//! (where main-only Web APIs like the DOM / `window` / `localStorage`
//! live) and getting the result back across the thread boundary.
use std::time::Duration;
use wasm_bindgen_test::*;
wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn spawn_on_main_from_main_runs_on_main() {
    let where_ran = wasmt::spawn_on_main(|| async { wasmt::thread_id() })
        .join()
        .await
        .unwrap();
    assert_eq!(where_ran, wasmt::ThreadId::Main);
}

#[wasm_bindgen_test]
async fn spawn_on_main_from_worker_runs_on_main() {
    // The outer task runs on a pool worker; it dispatches a closure to
    // main and receives the result back across threads.
    let h = wasmt::spawn(async {
        let here = wasmt::thread_id(); // a pool worker
        let there = wasmt::spawn_on_main(|| async { wasmt::thread_id() })
            .join()
            .await
            .unwrap();
        (here, there)
    });
    let (here, there) = h.join().await.unwrap();
    assert!(
        matches!(here, wasmt::ThreadId::Pool(_)),
        "outer task not on a pool worker: {here:?}"
    );
    assert_eq!(there, wasmt::ThreadId::Main, "closure did not run on main");
}

#[wasm_bindgen_test]
async fn spawn_on_main_delivers_value_and_can_await() {
    let h = wasmt::spawn(async {
        wasmt::spawn_on_main(|| async {
            // Runs on main's event loop; can await timers/promises there.
            wasmt::time::sleep(Duration::from_millis(20)).await;
            123u32
        })
        .join()
        .await
        .unwrap()
    });
    assert_eq!(h.join().await.unwrap(), 123);
}

#[wasm_bindgen_test]
async fn spawn_on_main_many_from_workers() {
    // Fan many worker tasks into main concurrently; all must complete.
    let mut handles = Vec::new();
    for i in 0..32u32 {
        handles.push(wasmt::spawn(async move {
            wasmt::spawn_on_main(move || async move { i * 2 })
                .join()
                .await
                .unwrap()
        }));
    }
    let mut total = 0u32;
    for h in handles {
        total += h.join().await.unwrap();
    }
    assert_eq!(total, (0..32u32).map(|i| i * 2).sum::<u32>());
}

#[wasm_bindgen_test]
async fn spawn_on_main_abort() {
    let h = wasmt::spawn_on_main(|| async {
        wasmt::time::sleep(Duration::from_secs(60)).await;
        1u32
    });
    let ab = h.abort_handle();
    wasmt::time::sleep(Duration::from_millis(20)).await;
    ab.abort();
    assert_eq!(h.join().await, Err(wasmt::JoinError::Cancelled));
}
