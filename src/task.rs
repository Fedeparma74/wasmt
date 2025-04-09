use futures::future::{AbortHandle, Abortable};
use js_sys::Promise;
use std::future::Future;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

use crate::worker;

pub fn spawn_blocking<T>(f: impl FnOnce() -> T + 'static) -> blocking::JoinHandle<T>
where
    T: 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    worker::spawn_blocking(move || {
        tx.send(f()).ok();
    });
    blocking::JoinHandle { rx }
}

pub fn spawn<F>(future: F) -> r#async::JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let abortable_future = Abortable::new(future, abort_registration);
    worker::spawn(async move {
        if let Ok(result) = abortable_future.await {
            tx.send(result).ok();
        }
    });
    r#async::JoinHandle {
        abort_handle,
        aborted: false,
        rx,
    }
}

#[wasm_bindgen(js_name = "spawnLocal")]
/// Runs a `Promise` on the current thread.
/// The promise will be scheduled to run in the background and cannot contain any stack references.
/// The promise will always be run on the next microtask tick.
pub fn js_spawn_local(promise: Promise) -> r#async::JsJoinHandle {
    let future = wasm_bindgen_futures::JsFuture::from(promise);
    let handle = spawn_local(future);
    r#async::JsJoinHandle { handle }
}

#[wasm_bindgen(js_name = "spawn")]
/// Runs a `Promise` on a new worker thread.
pub fn js_spawn(promise: Promise) -> r#async::JsJoinHandle {
    let future = wasm_bindgen_futures::JsFuture::from(promise);
    let handle = spawn(future);
    r#async::JsJoinHandle { handle }
}

pub fn spawn_local<F>(future: F) -> r#async::JoinHandle<F::Output>
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
    r#async::JoinHandle {
        abort_handle,
        aborted: false,
        rx,
    }
}

pub mod r#async {
    use futures::{future::FusedFuture, stream::AbortHandle};

    use super::*;

    pub struct JoinHandle<T> {
        pub(crate) abort_handle: AbortHandle,
        pub(crate) aborted: bool,
        pub(crate) rx: futures::channel::oneshot::Receiver<T>,
    }

    impl<T> JoinHandle<T> {
        pub async fn join(self) -> Result<T, JoinError> {
            self.rx.await.map_err(|_| {
                if self.aborted {
                    JoinError::Aborted
                } else {
                    JoinError::Panic
                }
            })
        }

        pub fn abort(&mut self) {
            self.abort_handle.abort();
            self.aborted = true;
            self.rx.close();
        }

        pub fn is_finished(&self) -> bool {
            self.rx.is_terminated()
        }
    }

    #[wasm_bindgen(js_name = "JoinHandle")]
    pub struct JsJoinHandle {
        pub(crate) handle: JoinHandle<Result<JsValue, JsValue>>,
    }

    #[wasm_bindgen(js_class = "JoinHandle")]
    impl JsJoinHandle {
        /// Awaits the result of the task on the current thread.
        #[wasm_bindgen]
        pub async fn join(self) -> Result<JsValue, JsValue> {
            match self.handle.join().await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(err)) => Err(err),
                Err(err) => Err(JsValue::from_str(&err.to_string())),
            }
        }

        /// Aborts the task.
        #[wasm_bindgen]
        pub fn abort(&mut self) {
            self.handle.abort();
        }
    }
}

pub mod blocking {
    use futures::future::FusedFuture;

    use super::*;

    pub struct JoinHandle<T> {
        pub(crate) rx: futures::channel::oneshot::Receiver<T>,
    }

    impl<T> JoinHandle<T> {
        pub async fn join(self) -> Result<T, JoinError> {
            self.rx.await.map_err(|_| JoinError::Panic)
        }

        pub fn is_finished(&self) -> bool {
            self.rx.is_terminated()
        }
    }
}

#[derive(PartialEq)]
pub enum JoinError {
    Aborted,
    Panic,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinError::Aborted => write!(f, "thread was aborted"),
            JoinError::Panic => write!(f, "thread panicked"),
        }
    }
}

impl std::fmt::Debug for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinError::Aborted => write!(f, "JoinError::Aborted"),
            JoinError::Panic => write!(f, "JoinError::Panic"),
        }
    }
}

impl std::error::Error for JoinError {}

impl From<JoinError> for JsValue {
    fn from(err: JoinError) -> Self {
        match err {
            JoinError::Aborted => JsValue::from_str("thread was aborted"),
            JoinError::Panic => JsValue::from_str("thread panicked"),
        }
    }
}

impl From<JoinError> for std::io::Error {
    fn from(err: JoinError) -> Self {
        match err {
            JoinError::Aborted => std::io::Error::other("thread was aborted"),
            JoinError::Panic => std::io::Error::other("thread panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::time::{sleep, sleep_blocking};

    use super::*;

    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(thread_local_v2, js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_spawn_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            sleep_blocking(Duration::from_millis(100));
            1
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_spawn_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_local(async move {
            sleep(Duration::from_millis(100)).await;
            1
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_spawn_blocking_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_blocking(|| {
            sleep_blocking(Duration::from_millis(100));
            1
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_task_in_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_local_task_in_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(100)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_blocking_task_in_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            let handle = spawn_blocking(|| {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await, Ok(1));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_task_in_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_local(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_local_task_in_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_local(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(100)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_blocking_task_in_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_local(async move {
            let handle = spawn_blocking(|| {
                sleep_blocking(Duration::from_millis(100));
                1
            });
            handle.join().await.unwrap()
        });
        assert_eq!(handle.join().await, Ok(1));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start >= 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let mut handle = spawn(async move {
            sleep_blocking(Duration::from_millis(1000));
            1
        });
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.is_finished());
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let mut handle = spawn_local(async move {
            sleep(Duration::from_millis(100)).await;
            1
        });
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.is_finished());
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 100.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_task_in_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            let mut handle = spawn(async move {
                sleep_blocking(Duration::from_millis(1000));
                1
            });
            assert!(!handle.is_finished());
            handle.abort();
            assert!(handle.is_finished());
            assert!(handle.aborted);
            assert!(handle.join().await == Err(JoinError::Aborted));
            1
        });
        assert!(!handle.is_finished());
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_task_in_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let mut handle = spawn_local(async move {
            let handle = spawn(async move {
                sleep_blocking(Duration::from_millis(1000));
                1
            });
            handle.join().await.unwrap()
        });
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.is_finished());
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_task_in_blocking_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_blocking(|| {
            futures::executor::block_on(async move {
                let mut handle = spawn(async move {
                    sleep_blocking(Duration::from_millis(1000));
                    1
                });
                assert!(!handle.is_finished());
                handle.abort();
                assert!(handle.is_finished());
                assert!(handle.aborted);
                assert!(handle.join().await == Err(JoinError::Aborted));
                1
            })
        });
        assert_eq!(handle.join().await, Ok(1));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_local_task_in_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn(async move {
            let mut handle = spawn_local(async move {
                sleep(Duration::from_millis(1000)).await;
                1
            });
            assert!(!handle.is_finished());
            handle.abort();
            assert!(handle.is_finished());
            assert!(handle.aborted);
            assert!(handle.join().await == Err(JoinError::Aborted));
            1
        });
        assert!(!handle.is_finished());
        assert_eq!(handle.join().await.unwrap(), 1);
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_local_task_in_local_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let mut handle = spawn_local(async move {
            let handle = spawn_local(async move {
                sleep(Duration::from_millis(1000)).await;
                1
            });
            handle.join().await.unwrap()
        });
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.is_finished());
        assert!(handle.aborted);
        assert!(handle.join().await == Err(JoinError::Aborted));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }

    #[wasm_bindgen_test]
    async fn test_abort_local_task_in_blocking_task() {
        let start = PERFORMANCE.with(|performance| performance.now());
        let handle = spawn_blocking(|| {
            futures::executor::block_on(async move {
                let mut handle = spawn_local(async move {
                    sleep(Duration::from_millis(1000)).await;
                    1
                });
                assert!(!handle.is_finished());
                handle.abort();
                assert!(handle.is_finished());
                assert!(handle.aborted);
                assert!(handle.join().await == Err(JoinError::Aborted));
                1
            })
        });
        assert_eq!(handle.join().await, Ok(1));
        let end = PERFORMANCE.with(|performance| performance.now());
        assert!(end - start < 1000.0);
    }
}
