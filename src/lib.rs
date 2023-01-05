pub mod thread;
pub mod time;
pub mod utils;
mod worker;

use futures::future::{AbortHandle, FusedFuture};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsValue;

#[cfg(not(target_arch = "wasm32"))]
compile_error!("This crate can only be compiled for wasm32-unknown-unknown target");
#[cfg(not(any(
    target_feature = "atomics",
    target_feature = "bulk-memory",
    target_feature = "mutable-globals"
)))]
compile_error!("Make sure to build std with `RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals'`");

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

pub struct JoinHandle<T> {
    abort_handle: AbortHandle,
    aborted: bool,
    rx: futures::channel::oneshot::Receiver<T>,
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
            JoinError::Aborted => {
                std::io::Error::new(std::io::ErrorKind::Other, "thread was aborted")
            }
            JoinError::Panic => std::io::Error::new(std::io::ErrorKind::Other, "thread panicked"),
        }
    }
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
