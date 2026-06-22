//! Browser tests for the native `wasmt::sync` primitives, with emphasis
//! on the main-thread + worker contention patterns that trapped under
//! the old `tokio::sync` re-export ("Atomics.wait cannot be called in
//! this context").
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use wasm_bindgen_test::*;
wasm_bindgen_test_configure!(run_in_browser);

// ---------- mpsc ----------

#[wasm_bindgen_test]
async fn mpsc_bounded_pingpong_main_consumer_high_volume() {
    // The exact shape that crashed under tokio: producer on a worker,
    // consumer on main, tiny buffer => heavy contention.
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<u32>(1);
    let producer = wasmt::spawn(async move {
        for i in 0..3000u32 {
            tx.send(i).await.unwrap();
        }
    });
    let mut expected = 0u32;
    while let Some(v) = rx.recv().await {
        assert_eq!(v, expected);
        expected += 1;
    }
    producer.join().await.unwrap();
    assert_eq!(expected, 3000);
}

#[wasm_bindgen_test]
async fn mpsc_multi_producer_fan_in() {
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<u32>(4);
    let mut producers = Vec::new();
    for p in 0..8u32 {
        let tx = tx.clone();
        producers.push(wasmt::spawn(async move {
            for i in 0..100u32 {
                tx.send(p * 1000 + i).await.unwrap();
            }
        }));
    }
    drop(tx);
    let mut count = 0u32;
    while rx.recv().await.is_some() {
        count += 1;
    }
    for p in producers {
        p.join().await.unwrap();
    }
    assert_eq!(count, 800);
}

#[wasm_bindgen_test]
async fn mpsc_unbounded_works() {
    let (tx, mut rx) = wasmt::sync::mpsc::unbounded_channel::<u32>();
    for i in 0..1000u32 {
        tx.send(i).unwrap();
    }
    drop(tx);
    let mut sum = 0u64;
    while let Some(v) = rx.recv().await {
        sum += v as u64;
    }
    assert_eq!(sum, (0..1000u64).sum::<u64>());
}

#[wasm_bindgen_test]
async fn mpsc_try_send_full_then_closed() {
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<u32>(1);
    tx.try_send(1).unwrap();
    assert!(matches!(
        tx.try_send(2),
        Err(wasmt::sync::mpsc::TrySendError::Full(2))
    ));
    assert_eq!(rx.recv().await, Some(1));
    drop(rx);
    assert!(matches!(
        tx.try_send(3),
        Err(wasmt::sync::mpsc::TrySendError::Closed(3))
    ));
}

// ---------- Mutex ----------

#[wasm_bindgen_test]
async fn mutex_contended_main_and_worker() {
    let m = Arc::new(wasmt::sync::Mutex::new(0u64));
    let m2 = m.clone();
    let worker = wasmt::spawn(async move {
        for _ in 0..10_000u32 {
            *m2.lock().await += 1;
        }
    });
    for _ in 0..10_000u32 {
        *m.lock().await += 1;
    }
    worker.join().await.unwrap();
    assert_eq!(*m.lock().await, 20_000);
}

#[wasm_bindgen_test]
async fn mutex_try_lock() {
    let m = wasmt::sync::Mutex::new(5u32);
    let g = m.lock().await;
    assert!(m.try_lock().is_err());
    drop(g);
    assert_eq!(*m.try_lock().unwrap(), 5);
}

// ---------- RwLock ----------

#[wasm_bindgen_test]
async fn rwlock_readers_and_writer() {
    let lock = Arc::new(wasmt::sync::RwLock::new(0u64));
    // Many concurrent readers.
    {
        let _r1 = lock.read().await;
        let _r2 = lock.read().await;
        let _r3 = lock.read().await;
        assert!(lock.try_write().is_err());
    }
    let l2 = lock.clone();
    let w = wasmt::spawn(async move {
        for _ in 0..5000u32 {
            *l2.write().await += 1;
        }
    });
    for _ in 0..5000u32 {
        *lock.write().await += 1;
    }
    w.join().await.unwrap();
    assert_eq!(*lock.read().await, 10_000);
}

// ---------- Semaphore ----------

#[wasm_bindgen_test]
async fn semaphore_limits_concurrency() {
    let sem = Arc::new(wasmt::sync::Semaphore::new(2));
    let live = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for _ in 0..10u32 {
        let sem = sem.clone();
        let live = live.clone();
        let peak = peak.clone();
        handles.push(wasmt::spawn(async move {
            let _p = sem.acquire().await.unwrap();
            let n = live.fetch_add(1, Ordering::AcqRel) + 1;
            peak.fetch_max(n, Ordering::AcqRel);
            wasmt::time::sleep(Duration::from_millis(10)).await;
            live.fetch_sub(1, Ordering::AcqRel);
        }));
    }
    for h in handles {
        h.join().await.unwrap();
    }
    assert!(peak.load(Ordering::Acquire) <= 2, "exceeded permit cap");
}

#[wasm_bindgen_test]
async fn semaphore_cancelled_waiter_does_not_block_others() {
    // Regression: a cancelled waiter must be removed from the FIFO
    // queue, else it blocks the fast path and a later acquirer hangs
    // even though permits are free.
    let sem = Arc::new(wasmt::sync::Semaphore::new(2));
    let _held = sem.try_acquire().unwrap(); // 1 permit remains
    // Acquire 2 (more than free) and let it time out -> cancels mid-wait.
    let r =
        wasmt::time::timeout(Duration::from_millis(30), sem.clone().acquire_many_owned(2)).await;
    assert!(r.is_err(), "expected the over-large acquire to time out");
    // The 1 free permit must now be takeable (no stale waiter blocking).
    assert!(
        sem.try_acquire().is_ok(),
        "cancelled waiter left the free permit unreachable"
    );
}

#[wasm_bindgen_test]
async fn rwlock_cancelled_writer_does_not_block_readers() {
    let lock = Arc::new(wasmt::sync::RwLock::new(0u32));
    let _r = lock.read().await; // a live reader
    // A writer needs all permits; it waits behind the reader — cancel it.
    let w = wasmt::time::timeout(Duration::from_millis(30), lock.clone().write_owned()).await;
    assert!(w.is_err(), "writer should have timed out behind the reader");
    // Another reader must still acquire promptly.
    let r2 = wasmt::time::timeout(Duration::from_secs(2), lock.read()).await;
    assert!(r2.is_ok(), "reader hung behind a cancelled writer");
}

// ---------- Notify ----------

#[wasm_bindgen_test]
async fn notify_survives_cancelled_waiters() {
    // Regression: a notified() awaited then cancelled must remove its
    // node; afterwards the Notify must still behave correctly.
    let n = Arc::new(wasmt::sync::Notify::new());
    for _ in 0..100 {
        let _ = wasmt::time::timeout(Duration::from_millis(1), n.notified()).await;
    }
    n.notify_one();
    wasmt::time::timeout(Duration::from_secs(2), n.notified())
        .await
        .expect("Notify broken after cancel churn");
}

#[wasm_bindgen_test]
async fn notify_one_wakes_waiter() {
    let n = Arc::new(wasmt::sync::Notify::new());
    let n2 = n.clone();
    let woken = Arc::new(AtomicU32::new(0));
    let w2 = woken.clone();
    let task = wasmt::spawn(async move {
        n2.notified().await;
        w2.store(1, Ordering::Release);
    });
    // Give it time to park, then notify.
    wasmt::time::sleep(Duration::from_millis(30)).await;
    n.notify_one();
    task.join().await.unwrap();
    assert_eq!(woken.load(Ordering::Acquire), 1);
}

#[wasm_bindgen_test]
async fn notify_one_stores_permit() {
    let n = wasmt::sync::Notify::new();
    n.notify_one(); // no waiter yet -> stored
    // Should return immediately.
    wasmt::time::timeout(Duration::from_secs(2), n.notified())
        .await
        .expect("stored permit not consumed");
}

// ---------- oneshot ----------

#[wasm_bindgen_test]
async fn oneshot_cross_thread() {
    let (tx, rx) = wasmt::sync::oneshot::channel::<u32>();
    wasmt::spawn(async move {
        tx.send(99).unwrap();
    });
    assert_eq!(rx.await.unwrap(), 99);
}

#[wasm_bindgen_test]
async fn oneshot_sender_drop_is_err() {
    let (tx, rx) = wasmt::sync::oneshot::channel::<u32>();
    drop(tx);
    assert!(rx.await.is_err());
}

// ---------- broadcast ----------

#[wasm_bindgen_test]
async fn broadcast_fanout() {
    let (tx, mut r1) = wasmt::sync::broadcast::channel::<u32>(16);
    let mut r2 = tx.subscribe();
    for i in 0..10u32 {
        tx.send(i).unwrap();
    }
    drop(tx);
    let mut s1 = 0u32;
    while let Ok(v) = r1.recv().await {
        s1 += v;
    }
    let mut s2 = 0u32;
    while let Ok(v) = r2.recv().await {
        s2 += v;
    }
    assert_eq!(s1, (0..10u32).sum::<u32>());
    assert_eq!(s2, (0..10u32).sum::<u32>());
}

#[wasm_bindgen_test]
async fn broadcast_lagged() {
    let (tx, mut rx) = wasmt::sync::broadcast::channel::<u32>(2);
    for i in 0..5u32 {
        tx.send(i).unwrap();
    }
    // Capacity 2 => first recv reports lag of 3 and fast-forwards.
    match rx.recv().await {
        Err(wasmt::sync::broadcast::error::RecvError::Lagged(n)) => assert_eq!(n, 3),
        other => panic!("expected Lagged(3), got {other:?}"),
    }
    assert_eq!(rx.recv().await.unwrap(), 3);
    assert_eq!(rx.recv().await.unwrap(), 4);
}

// ---------- watch ----------

#[wasm_bindgen_test]
async fn watch_changed_and_borrow() {
    let (tx, mut rx) = wasmt::sync::watch::channel(0u32);
    assert_eq!(*rx.borrow(), 0);
    let producer = wasmt::spawn(async move {
        for i in 1..=5u32 {
            wasmt::time::sleep(Duration::from_millis(5)).await;
            tx.send(i).unwrap();
        }
    });
    let mut last = 0;
    while rx.changed().await.is_ok() {
        last = *rx.borrow_and_update();
    }
    producer.join().await.unwrap();
    assert_eq!(last, 5);
}

// ---------- OnceCell ----------

#[wasm_bindgen_test]
async fn once_cell_init_once() {
    let cell: Arc<wasmt::sync::OnceCell<u32>> = Arc::new(wasmt::sync::OnceCell::new());
    let inits = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for _ in 0..8u32 {
        let cell = cell.clone();
        let inits = inits.clone();
        handles.push(wasmt::spawn(async move {
            *cell
                .get_or_init(|| async {
                    inits.fetch_add(1, Ordering::AcqRel);
                    wasmt::time::sleep(Duration::from_millis(5)).await;
                    42u32
                })
                .await
        }));
    }
    for h in handles {
        assert_eq!(h.join().await.unwrap(), 42);
    }
    assert_eq!(
        inits.load(Ordering::Acquire),
        1,
        "initialized more than once"
    );
}

// ---------- Barrier ----------

#[wasm_bindgen_test]
async fn barrier_releases_together() {
    let barrier = Arc::new(wasmt::sync::Barrier::new(4));
    let leaders = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for _ in 0..4u32 {
        let barrier = barrier.clone();
        let leaders = leaders.clone();
        handles.push(wasmt::spawn(async move {
            let r = barrier.wait().await;
            if r.is_leader() {
                leaders.fetch_add(1, Ordering::AcqRel);
            }
        }));
    }
    for h in handles {
        h.join().await.unwrap();
    }
    assert_eq!(leaders.load(Ordering::Acquire), 1, "exactly one leader");
}
