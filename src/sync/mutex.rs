//! `Mutex` — a main-thread-safe async mutex, API-compatible with
//! `tokio::sync::Mutex`. Built on [`super::Semaphore`] (one permit).

use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use super::semaphore::Semaphore;

/// Error returned by [`Mutex::try_lock`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct TryLockError(());

impl TryLockError {
    pub(crate) fn new() -> Self {
        TryLockError(())
    }
}

impl fmt::Display for TryLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation would block")
    }
}
impl std::error::Error for TryLockError {}

/// An asynchronous mutex. Mirrors `tokio::sync::Mutex`.
pub struct Mutex<T: ?Sized> {
    sem: Semaphore,
    data: UnsafeCell<T>,
}

// SAFETY: the single semaphore permit guarantees exclusive access to
// `data`; the permit can be released cross-thread, so `T: Send` makes
// the `Mutex` both `Send` and `Sync`.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Mutex {
            sem: Semaphore::new(1),
            data: UnsafeCell::new(value),
        }
    }

    /// Consume the mutex, returning the inner value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Lock the mutex, waiting until it is available.
    pub async fn lock(&self) -> MutexGuard<'_, T> {
        let permit = self.sem.acquire().await.expect("mutex never closed");
        permit.forget();
        MutexGuard { lock: self }
    }

    /// Try to lock without waiting.
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        match self.sem.try_acquire() {
            Ok(p) => {
                p.forget();
                Ok(MutexGuard { lock: self })
            }
            Err(_) => Err(TryLockError(())),
        }
    }

    /// Lock, returning a guard that holds an `Arc` to the mutex.
    pub async fn lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        let permit = self.sem.acquire().await.expect("mutex never closed");
        permit.forget();
        OwnedMutexGuard { lock: self }
    }

    /// Try to lock (owned) without waiting.
    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, TryLockError> {
        // Forget the permit first so its borrow of `self` ends before
        // we move `self` into the guard.
        match self.sem.try_acquire() {
            Ok(p) => p.forget(),
            Err(_) => return Err(TryLockError(())),
        }
        Ok(OwnedMutexGuard { lock: self })
    }

    /// Exclusive access without locking (compile-time exclusivity).
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn unlock(&self) {
        self.sem.add_permits(1);
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Mutex::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").finish_non_exhaustive()
    }
}

/// RAII guard from [`Mutex::lock`].
pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the only permit.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the only permit.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}
impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
impl<T: ?Sized + fmt::Display> fmt::Display for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// RAII guard from [`Mutex::lock_owned`].
pub struct OwnedMutexGuard<T: ?Sized> {
    lock: Arc<Mutex<T>>,
}

impl<T: ?Sized> OwnedMutexGuard<T> {
    /// The `Arc<Mutex>` this guard came from.
    pub fn mutex(this: &Self) -> &Arc<Mutex<T>> {
        &this.lock
    }
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the only permit.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the only permit.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for OwnedMutexGuard<T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}
impl<T: ?Sized + fmt::Debug> fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
