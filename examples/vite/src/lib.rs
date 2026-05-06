//! Minimal Vite-driven sample exercising every spawn primitive.

use std::time::Duration;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub async fn start() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);

    log::info!(
        "wasmt sample: hardware concurrency = {}",
        navigator_hardware_concurrency()
    );

    // 1. Send work on the work-stealing pool.
    let send = wasmt::spawn(async {
        wasmt::time::sleep(Duration::from_millis(50)).await;
        "send result"
    });

    // 2. Blocking work on a dedicated worker.
    let blocking = wasmt::spawn_blocking(|| {
        std::thread::sleep(Duration::from_millis(40));
        "blocking result"
    });

    // 3. !Send work pinned to a pool worker — uses a JsValue across an await.
    let pinned = wasmt::spawn_pinned(|| async {
        let promise = js_sys::Promise::resolve(&JsValue::from(7u32));
        let v = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
        format!("pinned result {}", v.as_f64().unwrap() as u32)
    });

    // 4. spawn_local for main-thread !Send work.
    let local = wasmt::spawn_local(async {
        wasmt::time::sleep(Duration::from_millis(30)).await;
        "local result"
    });

    log::info!("send:     {}", send.join().await.unwrap());
    log::info!("blocking: {}", blocking.join().await.unwrap());
    log::info!("pinned:   {}", pinned.join().await.unwrap());
    log::info!("local:    {}", local.join().await.unwrap());

    // 5. JoinSet of mixed work.
    let mut set: wasmt::JoinSet<u32> = wasmt::JoinSet::new();
    for i in 0..8u32 {
        set.spawn(async move {
            wasmt::time::sleep(Duration::from_millis(10)).await;
            i * i
        });
    }
    let mut sum = 0u32;
    while let Some((_id, r)) = set.join_next().await {
        sum += r.unwrap();
    }
    log::info!("sum of squares 0..8 = {sum}");

    // 6. mpsc channel via wasmt::sync (re-export of tokio::sync).
    let (tx, mut rx) = wasmt::sync::mpsc::channel::<&'static str>(4);
    wasmt::spawn(async move {
        for msg in ["alpha", "beta", "gamma"] {
            tx.send(msg).await.unwrap();
        }
    });
    while let Some(msg) = rx.recv().await {
        log::info!("got msg: {msg}");
    }

    log::info!("all done");
}

fn navigator_hardware_concurrency() -> u32 {
    js_sys::global()
        .dyn_into::<web_sys::WorkerGlobalScope>()
        .ok()
        .map(|s| s.navigator().hardware_concurrency() as u32)
        .or_else(|| {
            js_sys::global()
                .dyn_into::<web_sys::Window>()
                .ok()
                .map(|w| w.navigator().hardware_concurrency() as u32)
        })
        .unwrap_or(1)
}
