//! `OnceCell` — a main-thread-safe async once-cell, API-compatible with
//! `tokio::sync::OnceCell`. Uses a [`Semaphore`] as the one-time init
//! lock so concurrent initializers serialize without parking the OS
//! thread.

use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use super::semaphore::Semaphore;

/// Error returned by [`OnceCell::set`].
#[derive(Debug, PartialEq, Eq)]
pub enum SetError<T> {
    /// The cell already holds a value.
    AlreadyInitialized(T),
    /// Another initialization is in progress.
    Initializing(T),
}

impl<T: fmt::Debug> fmt::Display for SetError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetError::AlreadyInitialized(_) => f.write_str("already initialized"),
            SetError::Initializing(_) => f.write_str("currently initializing"),
        }
    }
}
impl<T: fmt::Debug> std::error::Error for SetError<T> {}

/// An async cell initialized at most once. Mirrors
/// `tokio::sync::OnceCell`.
pub struct OnceCell<T> {
    init: Semaphore,
    set: AtomicBool,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: writes happen once under the init permit, then `set` is
// published; readers only ever observe an immutable, fully-written value.
unsafe impl<T: Send> Send for OnceCell<T> {}
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}

impl<T> OnceCell<T> {
    pub fn new() -> Self {
        OnceCell {
            init: Semaphore::new(1),
            set: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    /// Create an already-initialized cell.
    pub fn new_with(value: T) -> Self {
        OnceCell {
            init: Semaphore::new(1),
            set: AtomicBool::new(true),
            value: UnsafeCell::new(Some(value)),
        }
    }

    pub fn initialized(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    /// Borrow the value if initialized.
    pub fn get(&self) -> Option<&T> {
        if self.set.load(Ordering::Acquire) {
            // SAFETY: `set` published the fully-written value; it is
            // never mutated again, so a shared ref is sound.
            unsafe { (*self.value.get()).as_ref() }
        } else {
            None
        }
    }

    /// Set the value if not yet initialized.
    pub fn set(&self, value: T) -> Result<(), SetError<T>> {
        if self.set.load(Ordering::Acquire) {
            return Err(SetError::AlreadyInitialized(value));
        }
        match self.init.try_acquire() {
            Ok(permit) => {
                if self.set.load(Ordering::Acquire) {
                    return Err(SetError::AlreadyInitialized(value));
                }
                // SAFETY: we hold the init permit; no other writer.
                unsafe {
                    *self.value.get() = Some(value);
                }
                self.set.store(true, Ordering::Release);
                drop(permit);
                Ok(())
            }
            Err(_) => Err(SetError::Initializing(value)),
        }
    }

    /// Get the value, initializing it with `init` if necessary.
    pub async fn get_or_init<F, Fut>(&self, init: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if let Some(v) = self.get() {
            return v;
        }
        let permit = self.init.acquire().await.expect("once cell never closed");
        if self.set.load(Ordering::Acquire) {
            drop(permit);
            return self.get().expect("initialized");
        }
        let value = init().await;
        // SAFETY: we hold the init permit; no other writer.
        unsafe {
            *self.value.get() = Some(value);
        }
        self.set.store(true, Ordering::Release);
        drop(permit);
        self.get().expect("just initialized")
    }

    /// Like [`get_or_init`](Self::get_or_init) but the initializer may
    /// fail; on failure the cell stays uninitialized.
    pub async fn get_or_try_init<E, F, Fut>(&self, init: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(v) = self.get() {
            return Ok(v);
        }
        let permit = self.init.acquire().await.expect("once cell never closed");
        if self.set.load(Ordering::Acquire) {
            drop(permit);
            return Ok(self.get().expect("initialized"));
        }
        let value = init().await?;
        // SAFETY: we hold the init permit; no other writer.
        unsafe {
            *self.value.get() = Some(value);
        }
        self.set.store(true, Ordering::Release);
        drop(permit);
        Ok(self.get().expect("just initialized"))
    }

    /// Take the value out, leaving the cell uninitialized.
    pub fn take(&mut self) -> Option<T> {
        if *self.set.get_mut() {
            *self.set.get_mut() = false;
            self.value.get_mut().take()
        } else {
            None
        }
    }

    /// Consume the cell, returning the inner value if initialized.
    pub fn into_inner(mut self) -> Option<T> {
        self.value.get_mut().take()
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        OnceCell::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnceCell")
            .field("value", &self.get())
            .finish()
    }
}
