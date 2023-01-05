use futures::future::{AbortHandle, Abortable};
use futures::Future;

use crate::{worker, JoinHandle};

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let abortable_future = Abortable::new(future, abort_registration);
    worker::spawn(async move {
        if let Ok(result) = abortable_future.await {
            tx.send(result).ok();
        }
    });
    JoinHandle {
        abort_handle,
        aborted: false,
        rx,
    }
}

pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let abortable_future = Abortable::new(future, abort_registration);
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(result) = abortable_future.await {
            tx.send(result).ok();
        }
    });
    JoinHandle {
        abort_handle,
        aborted: false,
        rx,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        time::{sleep, sleep_blocking},
        JoinError,
    };

    use super::*;

    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_thread_spawn() {
        let start = PERFORMANCE.now();
        let handle = spawn(async move {
            sleep_blocking(Duration::from_millis(100));
            1
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_spawn_local() {
        let start = PERFORMANCE.now();
        let handle = spawn_local(async move {
            sleep(Duration::from_millis(100)).await;
            1
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_in_thread() {
        let start = PERFORMANCE.now();
        let handle = spawn(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_local_in_thread() {
        let start = PERFORMANCE.now();
        let handle = spawn(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(100)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_in_thread_local() {
        let start = PERFORMANCE.now();
        let handle = spawn_local(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_local_in_thread_local() {
        let start = PERFORMANCE.now();
        let handle = spawn_local(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(100)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort() {
        let start = PERFORMANCE.now();
        let mut handle = spawn(async move {
            sleep_blocking(Duration::from_millis(1000));
            1
        });
        assert_eq!(handle.is_finished(), false);
        handle.abort();
        assert_eq!(handle.is_finished(), true);
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.now();
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort_local() {
        let start = PERFORMANCE.now();
        let mut handle = spawn_local(async move {
            sleep(Duration::from_millis(100)).await;
            1
        });
        assert_eq!(handle.is_finished(), false);
        handle.abort();
        assert_eq!(handle.is_finished(), true);
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.now();
        assert!(end - start < 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort_in_thread() {
        let start = PERFORMANCE.now();
        let handle = spawn(async move {
            let mut handle = spawn(async move {
                sleep_blocking(Duration::from_millis(1000));
                1
            });
            assert_eq!(handle.is_finished(), false);
            handle.abort();
            assert_eq!(handle.is_finished(), true);
            assert!(handle.aborted);
            assert!(handle.join().await == Err(JoinError::Aborted));
            1
        });
        assert_eq!(handle.is_finished(), false);
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort_local_in_thread() {
        let start = PERFORMANCE.now();
        let handle = spawn(async move {
            let mut handle = spawn_local(async move {
                sleep(Duration::from_millis(1000)).await;
                1
            });
            assert_eq!(handle.is_finished(), false);
            handle.abort();
            assert_eq!(handle.is_finished(), true);
            assert!(handle.aborted);
            assert!(handle.join().await == Err(JoinError::Aborted));
            1
        });
        assert_eq!(handle.is_finished(), false);
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.now();
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort_in_thread_local() {
        let start = PERFORMANCE.now();
        let mut handle = spawn_local(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(1000));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.is_finished(), false);
        handle.abort();
        assert_eq!(handle.is_finished(), true);
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.now();
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_thread_abort_local_in_thread_local() {
        let start = PERFORMANCE.now();
        let mut handle = spawn_local(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(1000)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.is_finished(), false);
        handle.abort();
        assert_eq!(handle.is_finished(), true);
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.now();
        assert!(end - start < 1000.0);
    }
}
