use std::time::Duration;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::wasm_bindgen;
use web_sys::{Window, WorkerGlobalScope};

pub async fn sleep(dur: Duration) {
    let ms = dur.as_millis().min(i32::MAX as u128) as i32;
    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        if let Some(window) = global.dyn_ref::<Window>() {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                .expect("failed to set timeout");
        } else if let Some(scope) = global.dyn_ref::<WorkerGlobalScope>() {
            scope
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
                .expect("failed to set timeout");
        } else {
            panic!("unsupported global scope: expected Window or WorkerGlobalScope");
        }
    }))
    .await
    .expect("failed to sleep");
}

#[wasm_bindgen(js_name = "sleepMs")]
pub async fn sleep_ms(ms: u32) {
    sleep(Duration::from_millis(ms as u64)).await;
}

pub fn sleep_blocking(dur: Duration) {
    std::thread::sleep(dur);
}

#[wasm_bindgen(js_name = "sleepBlockingMs")]
pub fn sleep_blocking_ms(ms: u32) {
    sleep_blocking(Duration::from_millis(ms as u64));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(thread_local_v2, js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_sleep() {
        let start = PERFORMANCE.with(|performance| performance.now());
        sleep(Duration::from_millis(100)).await;
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_sleep_ms() {
        let start = PERFORMANCE.with(|performance| performance.now());
        sleep_ms(100).await;
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[cfg(all(
        target_feature = "atomics",
        target_feature = "bulk-memory",
        target_feature = "mutable-globals"
    ))]
    #[wasm_bindgen_test]
    async fn test_sleep_blocking() {
        let handle = crate::task::spawn(async move {
            let start = PERFORMANCE.with(|performance| performance.now());
            sleep_blocking(Duration::from_millis(100));
            let end = PERFORMANCE.with(|performance| performance.now());
            end - start
        });
        assert!(handle.join().await.unwrap() >= 100.0);
    }

    #[cfg(all(
        target_feature = "atomics",
        target_feature = "bulk-memory",
        target_feature = "mutable-globals"
    ))]
    #[wasm_bindgen_test]
    async fn test_sleep_blocking_ms() {
        let handle = crate::task::spawn(async move {
            let start = PERFORMANCE.with(|performance| performance.now());
            sleep_blocking_ms(100);
            let end = PERFORMANCE.with(|performance| performance.now());
            end - start
        });
        assert!(handle.join().await.unwrap() >= 100.0);
    }
}
