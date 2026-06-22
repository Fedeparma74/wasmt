//! `Barrier` — a main-thread-safe reimplementation of
//! `tokio::sync::Barrier`.
//!
//! Built on [`super::watch`] rather than [`super::Notify`]: the release
//! is signalled by bumping a generation counter, and `watch` is
//! level-triggered (compares versions), so a waiter that registers
//! *after* the leader fires still observes the change. An
//! edge-triggered `Notify` would drop that wakeup and hang the late
//! waiter.

use super::spin::Spin;
use super::watch;

/// A barrier that releases all waiters once `n` of them have arrived.
pub struct Barrier {
    n: usize,
    arrived: Spin<usize>,
    gen_tx: watch::Sender<u64>,
}

/// Result of [`Barrier::wait`]; exactly one waiter per generation is the
/// leader.
#[derive(Debug, Clone, Copy)]
pub struct BarrierWaitResult(bool);

impl BarrierWaitResult {
    /// `true` for exactly one task in the group.
    pub fn is_leader(&self) -> bool {
        self.0
    }
}

impl Barrier {
    pub fn new(n: usize) -> Self {
        let (gen_tx, _rx) = watch::channel(0u64);
        Barrier {
            // A zero-sized barrier releases immediately; clamp so the
            // arithmetic below is well-defined.
            n: n.max(1),
            arrived: Spin::new(0),
            gen_tx,
        }
    }

    /// Wait until `n` tasks have called `wait`.
    pub async fn wait(&self) -> BarrierWaitResult {
        // Subscribe *before* counting ourselves in, so the generation
        // we'd wait for is captured prior to any leader bumping it.
        let mut rx = self.gen_tx.subscribe();

        let is_leader = {
            let mut arrived = self.arrived.lock();
            *arrived += 1;
            if *arrived >= self.n {
                *arrived = 0;
                true
            } else {
                false
            }
        };

        if is_leader {
            // Releasing the whole group: bump the generation. All
            // waiters (which subscribed before incrementing `arrived`,
            // and thus before this send) observe the version change.
            self.gen_tx.send_modify(|g| *g = g.wrapping_add(1));
            BarrierWaitResult(true)
        } else {
            // Level-triggered: even if the leader already sent, this
            // returns immediately because the version differs.
            let _ = rx.changed().await;
            BarrierWaitResult(false)
        }
    }
}
