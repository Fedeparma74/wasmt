use std::time::Duration;

use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsCast;
use web_sys::{Window, WorkerGlobalScope};

pub async fn sleep(dur: Duration) {
    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
        match js_sys::global().dyn_into::<Window>() {
            Ok(window) => window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    &resolve,
                    dur.as_millis() as i32,
                )
                .expect("failed to set timeout"),
            Err(_) => {
                let worker_scope = js_sys::global().dyn_into::<WorkerGlobalScope>().unwrap();
                worker_scope
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        &resolve,
                        dur.as_millis() as i32,
                    )
                    .expect("failed to set timeout")
            }
        };
    }))
    .await
    .expect("failed to sleep");
}

#[wasm_bindgen]
pub async fn sleep_ms(ms: u32) {
    sleep(Duration::from_millis(ms as u64)).await;
}

pub fn sleep_blocking(dur: Duration) {
    std::thread::sleep(dur);
}

#[wasm_bindgen]
pub fn sleep_blocking_ms(ms: u32) {
    sleep_blocking(Duration::from_millis(ms as u64));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::task;

    use super::*;

    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_sleep() {
        let start = PERFORMANCE.now();
        sleep(Duration::from_millis(100)).await;
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_sleep_ms() {
        let start = PERFORMANCE.now();
        sleep_ms(100).await;
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_sleep_blocking() {
        let handle = task::spawn(async move {
            let start = PERFORMANCE.now();
            sleep_blocking(Duration::from_millis(100));
            let end = PERFORMANCE.now();
            end - start
        });
        assert!(handle.join().await.unwrap() >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_sleep_blocking_ms() {
        let handle = task::spawn(async move {
            let start = PERFORMANCE.now();
            sleep_blocking_ms(100);
            let end = PERFORMANCE.now();
            end - start
        });
        assert!(handle.join().await.unwrap() >= 100.0);
    }
}
