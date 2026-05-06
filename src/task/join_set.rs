//! Owned collection of tasks that can be awaited as a stream.
//!
//! Mirrors `tokio::task::JoinSet`. Each spawned task carries its
//! globally-unique [`super::Id`]; [`JoinSet::join_next`] returns
//! `(Id, Result<T, JoinError>)` so the caller can identify which task
//! produced which output (matches Tokio's API exactly).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{AbortHandle, Id, JoinError, JoinHandle};

struct Entry<T: Send + 'static> {
    handle: JoinHandle<T>,
    abort: AbortHandle,
}

/// A collection of join handles. Tasks added with
/// [`JoinSet::spawn`] / [`JoinSet::spawn_blocking`] /
/// [`JoinSet::spawn_local`] / [`JoinSet::spawn_pinned`] can be
/// drained in completion order via [`JoinSet::join_next`].
///
/// Dropping the `JoinSet` calls [`AbortHandle::abort`] on every live
/// task — matches `tokio::task::JoinSet`'s "cancel on drop" semantics.
pub struct JoinSet<T: Send + 'static> {
    entries: HashMap<Id, Entry<T>>,
}

impl<T: Send + 'static> Default for JoinSet<T> {
    fn default() -> Self {
        JoinSet::new()
    }
}

impl<T: Send + 'static> JoinSet<T> {
    pub fn new() -> Self {
        JoinSet {
            entries: HashMap::new(),
        }
    }

    /// Spawn a `Send + 'static` future and track its handle.
    /// Returns the task's globally-unique [`Id`].
    pub fn spawn<F>(&mut self, future: F) -> Id
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.insert(crate::task::spawn(future))
    }

    /// Spawn a blocking closure and track its handle.
    pub fn spawn_blocking<F>(&mut self, f: F) -> Id
    where
        F: FnOnce() -> T + 'static,
    {
        self.insert(crate::task::spawn_blocking(f))
    }

    /// Spawn a `'static` (possibly `!Send`) future on the local
    /// executor and track its handle.
    pub fn spawn_local<F>(&mut self, future: F) -> Id
    where
        F: Future<Output = T> + 'static,
    {
        // `wasmt::task::spawn_local` returns `LocalJoinHandle<T>` so
        // it can accept `!Send` outputs in general; the `JoinSet<T>`
        // outer bound (`T: Send + 'static`) lets us upcast it to a
        // `JoinHandle<T>` here for uniform storage in the set.
        self.insert(crate::task::spawn_local(future).into())
    }

    /// Spawn a `!Send` future on the runtime worker pool (pinned).
    pub fn spawn_pinned<F, Fut>(&mut self, f: F) -> Id
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output = T> + 'static,
    {
        self.insert(crate::task::spawn_pinned(f))
    }

    fn insert(&mut self, handle: JoinHandle<T>) -> Id {
        let id = handle.id();
        let abort = handle.abort_handle();
        self.entries.insert(id, Entry { handle, abort });
        id
    }

    /// Number of tasks still tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Abort all tasks. They observe the cancellation at their next
    /// yield point; blocking tasks ignore it.
    pub fn abort_all(&self) {
        for entry in self.entries.values() {
            entry.abort.abort();
        }
    }

    /// Cancel and forget every task without waiting.
    pub fn detach_all(&mut self) {
        self.entries.clear();
    }

    /// Wait for the next task in the set to complete, returning its
    /// id and result. Returns `None` once the set is empty.
    pub async fn join_next(&mut self) -> Option<(Id, Result<T, JoinError>)> {
        if self.entries.is_empty() {
            return None;
        }
        std::future::poll_fn(|cx| self.poll_join_next(cx)).await
    }

    fn poll_join_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(Id, Result<T, JoinError>)>> {
        let mut ready: Option<(Id, Result<T, JoinError>)> = None;
        for (id, entry) in self.entries.iter_mut() {
            // `JoinHandle<T>` is `Unpin` (all fields are `Unpin` and
            // there's no self-referential state), so we can poll it
            // through a `Pin::new` without `unsafe`.
            if let Poll::Ready(result) = Pin::new(&mut entry.handle).poll(cx) {
                ready = Some((*id, result));
                break;
            }
        }
        match ready {
            Some((id, result)) => {
                self.entries.remove(&id);
                Poll::Ready(Some((id, result)))
            }
            None if self.entries.is_empty() => Poll::Ready(None),
            None => Poll::Pending,
        }
    }
}

impl<T: Send + 'static> Drop for JoinSet<T> {
    fn drop(&mut self) {
        for entry in self.entries.values() {
            entry.abort.abort();
        }
    }
}
