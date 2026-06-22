//! `RwLock` — a main-thread-safe async reader/writer lock,
//! API-compatible with `tokio::sync::RwLock`. Built on
//! [`super::Semaphore`]: a reader takes one permit, a writer takes all
//! [`MAX_READS`] permits (matching tokio's design).

use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use super::mutex::TryLockError;
use super::semaphore::Semaphore;

/// Maximum concurrent readers. A writer acquires all of them at once.
const MAX_READS: usize = Semaphore::MAX_PERMITS;

/// An asynchronous reader-writer lock. Mirrors `tokio::sync::RwLock`.
pub struct RwLock<T: ?Sized> {
    sem: Semaphore,
    data: UnsafeCell<T>,
}

// SAFETY: permits gate access — readers share `&T` (needs `T: Sync`),
// writers get `&mut T`. Sending across threads needs `T: Send`.
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        RwLock {
            sem: Semaphore::new(MAX_READS),
            data: UnsafeCell::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        self.sem
            .acquire()
            .await
            .expect("rwlock never closed")
            .forget();
        RwLockReadGuard { lock: self }
    }

    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        match self.sem.try_acquire() {
            Ok(p) => {
                p.forget();
                Ok(RwLockReadGuard { lock: self })
            }
            Err(_) => Err(super::mutex::TryLockError::new()),
        }
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.sem
            .acquire_many(MAX_READS as u32)
            .await
            .expect("rwlock never closed")
            .forget();
        RwLockWriteGuard { lock: self }
    }

    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        match self.sem.try_acquire_many(MAX_READS as u32) {
            Ok(p) => {
                p.forget();
                Ok(RwLockWriteGuard { lock: self })
            }
            Err(_) => Err(super::mutex::TryLockError::new()),
        }
    }

    pub async fn read_owned(self: Arc<Self>) -> OwnedRwLockReadGuard<T> {
        self.sem
            .acquire()
            .await
            .expect("rwlock never closed")
            .forget();
        OwnedRwLockReadGuard { lock: self }
    }

    pub async fn write_owned(self: Arc<Self>) -> OwnedRwLockWriteGuard<T> {
        self.sem
            .acquire_many(MAX_READS as u32)
            .await
            .expect("rwlock never closed")
            .forget();
        OwnedRwLockWriteGuard { lock: self }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn release_read(&self) {
        self.sem.add_permits(1);
    }
    fn release_write(&self) {
        self.sem.add_permits(MAX_READS);
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        RwLock::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

// ---- borrowed guards ----

pub struct RwLockReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}
impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a read permit guarantees no writer is active.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_read();
    }
}
impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}
impl<'a, T: ?Sized> RwLockWriteGuard<'a, T> {
    /// Atomically downgrade a write guard into a read guard.
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        let lock = self.lock;
        // Release all-but-one permit so concurrent readers can proceed
        // while we retain read access. Skip this guard's Drop.
        std::mem::forget(self);
        lock.sem.add_permits(MAX_READS - 1);
        RwLockReadGuard { lock }
    }
}
impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a write permit guarantees exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: a write permit guarantees exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_write();
    }
}
impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// ---- owned guards ----

pub struct OwnedRwLockReadGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
}
impl<T: ?Sized> Deref for OwnedRwLockReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a read permit guarantees no writer is active.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for OwnedRwLockReadGuard<T> {
    fn drop(&mut self) {
        self.lock.release_read();
    }
}

pub struct OwnedRwLockWriteGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
}
impl<T: ?Sized> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a write permit guarantees exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: a write permit guarantees exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for OwnedRwLockWriteGuard<T> {
    fn drop(&mut self) {
        self.lock.release_write();
    }
}
