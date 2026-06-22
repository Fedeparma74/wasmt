//! Async synchronization primitives that are **safe to use from the
//! browser main thread**.
//!
//! # Why this module exists
//!
//! `wasmt` originally re-exported `tokio::sync`. That is unsound on
//! `wasm32-unknown-unknown`: every `tokio::sync` primitive (and every
//! alternative built on `futures::lock` / `event-listener` /
//! `async-lock`) protects its internal waiter bookkeeping with a
//! `std::sync::Mutex`. On wasm a *contended* `std::sync::Mutex` blocks
//! via `memory.atomic.wait32`, and **`Atomics.wait` is illegal on the
//! main thread** — it throws `RuntimeError: Atomics.wait cannot be
//! called in this context` and traps the wasm instance. So any channel
//! / mutex / notify shared between a worker task and main-thread code
//! crashed intermittently under contention.
//!
//! These primitives instead guard their internal state with a
//! non-parking [`spin`] lock and wait via lock-free
//! [`futures::task::AtomicWaker`] cells, so they never call
//! `Atomics.wait`. They are API-compatible with the `tokio::sync`
//! subset they replace.

mod spin;

pub mod semaphore;

pub mod oneshot;

pub mod mpsc;

mod mutex;
mod notify;
mod once_cell;
mod rwlock;

pub mod broadcast;
pub mod watch;

pub use mutex::{Mutex, MutexGuard, OwnedMutexGuard};
pub use notify::Notify;
pub use once_cell::{OnceCell, SetError};
pub use rwlock::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
pub use semaphore::{
    AcquireError, OwnedSemaphorePermit, Semaphore, SemaphorePermit, TryAcquireError,
};

mod barrier;
pub use barrier::{Barrier, BarrierWaitResult};
