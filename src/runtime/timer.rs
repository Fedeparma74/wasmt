//! Async timer driver.
//!
//! All [`crate::time::sleep`] / [`crate::time::timeout`] timers share
//! a single driver. Producers (any thread) push `Op::Insert` /
//! `Op::Cancel` messages into a lock-free [`SegQueue`]; the main
//! thread is the sole consumer, draining the queue into a private
//! [`BinaryHeap`] on every tick. This means:
//!
//! - The main thread never blocks on a Mutex (`Atomics.wait` is not
//!   legal there).
//! - Producers do not contend for a Mutex either — `SegQueue::push`
//!   is wait-free in the common path.
//! - Cancellations free the heap entry as soon as the next tick runs,
//!   so a long-deadline `timeout` that completes fast doesn't pin
//!   memory.
//!
//! When a producer's deadline beats the currently armed `setTimeout`,
//! the producer kicks the driver: a worker posts a
//! `wasmt_timer_kick` envelope through [`crate::runtime::main_bus`];
//! main calls [`__wasmt_timer_kick`] directly. Cross-thread waking is
//! safe — the registered `Waker` is whatever polled the [`Sleep`],
//! and runtime task wakers re-enqueue across threads via the
//! scheduler.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use crossbeam_queue::SegQueue;
use futures::task::AtomicWaker;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;

/// State shared between a [`Sleep`] future and its heap entry.
struct EntryState {
    /// Set by the driver when the deadline fires. Read by `Sleep::poll`.
    elapsed: AtomicBool,
    /// The most recently registered waker. The driver fires this on
    /// expiration; if the future was polled by the runtime scheduler,
    /// the waker re-enqueues the task across threads.
    waker: AtomicWaker,
}

struct Entry {
    deadline_ms: u64,
    seq: u64,
    state: Arc<EntryState>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (self.deadline_ms, self.seq) == (other.deadline_ms, other.seq)
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.deadline_ms
            .cmp(&o.deadline_ms)
            .then(self.seq.cmp(&o.seq))
    }
}

/// Producer-to-driver message. Any thread pushes; main drains.
enum Op {
    Insert(Entry),
    Cancel(u64),
}

struct DriverInner {
    /// Lock-free MPSC ingress for inserts and cancels.
    ops: SegQueue<Op>,
    next_seq: AtomicU64,
    /// `Date.now()` ms of the deadline currently armed via
    /// `setTimeout`, or `i64::MAX` if no timeout is armed. Producers
    /// read this to decide whether they need to kick.
    scheduled_deadline_ms: AtomicI64,
}

static DRIVER: OnceLock<Arc<DriverInner>> = OnceLock::new();

fn driver() -> &'static Arc<DriverInner> {
    DRIVER.get_or_init(|| {
        Arc::new(DriverInner {
            ops: SegQueue::new(),
            next_seq: AtomicU64::new(1),
            scheduled_deadline_ms: AtomicI64::new(i64::MAX),
        })
    })
}

/// Main-only state.
struct MainState {
    /// Persistent `setTimeout` callback so we don't allocate a new
    /// Closure on every reschedule.
    callback: Closure<dyn FnMut()>,
    /// Id returned by the most recently armed `setTimeout`, so we can
    /// `clearTimeout` it on rearm.
    timeout_id: Option<i32>,
    /// The driver's authoritative pending-entries heap. Owned by main;
    /// untouched by workers.
    heap: BinaryHeap<Reverse<Entry>>,
    /// Set of cancelled `seq`s observed before their corresponding
    /// `Insert` was drained. Drained at the same time as the heap.
    pending_cancels: std::collections::HashSet<u64>,
}

thread_local! {
    static MAIN_STATE: RefCell<Option<MainState>> = const { RefCell::new(None) };
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn now_u64() -> u64 {
    now_ms() as u64
}

fn ensure_main_state() {
    MAIN_STATE.with(|m| {
        if m.borrow().is_some() {
            return;
        }
        let cb = Closure::<dyn FnMut()>::new(tick);
        *m.borrow_mut() = Some(MainState {
            callback: cb,
            timeout_id: None,
            heap: BinaryHeap::new(),
            pending_cancels: std::collections::HashSet::new(),
        });
    });
}

/// Drain the producer queue into the heap, collect any expired
/// entries, then fire their wakers and rearm `setTimeout`. Runs only
/// on main.
///
/// Wakers are fired *outside* the `MAIN_STATE` borrow so a waker
/// that synchronously schedules a new sleep cannot re-enter and
/// panic on double-borrow. If such a re-entry happens, the nested
/// `tick` will drain the new ops and arm a (possibly tighter) timer;
/// our `schedule` recomputes the next deadline from the current heap
/// rather than trusting a value captured before wakers fired, so we
/// don't clobber a tighter armed timer with a stale value.
fn tick() {
    ensure_main_state();
    let driver = driver();

    // Phase 1: drain ops + collect expired entries under the borrow.
    let expired = MAIN_STATE.with(|m| {
        let mut state = m.borrow_mut();
        let st = state.as_mut().expect("MAIN_STATE initialised");

        while let Some(op) = driver.ops.pop() {
            match op {
                Op::Insert(entry) => {
                    if st.pending_cancels.remove(&entry.seq) {
                        continue; // already cancelled; drop the entry
                    }
                    st.heap.push(Reverse(entry));
                }
                Op::Cancel(seq) => {
                    st.pending_cancels.insert(seq);
                }
            }
        }

        let now = now_u64();
        let mut expired: Vec<Arc<EntryState>> = Vec::new();
        loop {
            let action = match st.heap.peek() {
                None => Action::Stop,
                Some(Reverse(entry)) => {
                    if st.pending_cancels.contains(&entry.seq) {
                        Action::Cancel
                    } else if entry.deadline_ms <= now {
                        Action::Fire
                    } else {
                        Action::Stop
                    }
                }
            };
            match action {
                Action::Stop => break,
                Action::Cancel => {
                    let entry = st.heap.pop().unwrap().0;
                    st.pending_cancels.remove(&entry.seq);
                    drop(entry);
                }
                Action::Fire => {
                    let entry = st.heap.pop().unwrap().0;
                    entry.state.elapsed.store(true, Ordering::Release);
                    expired.push(entry.state);
                }
            }
        }

        // Lazy compaction: a `Cancel` for a non-top heap entry sits
        // in `pending_cancels` until that entry surfaces. Under a
        // long-tailed `timeout(...)` workload (many concurrent timers
        // that complete fast), this set grows linearly. Rebuild the
        // heap, dropping cancelled entries, when the set is large
        // relative to the heap. Runs only when the slack is
        // significant.
        //
        // Use `BinaryHeap::from(Vec)` for an O(n) heapify rather than
        // O(n log n) repeated `push`. We `mem::take` the old heap so
        // the iterator yields owned `Entry`s in one shot, filter, and
        // hand the resulting Vec to `from(...)`.
        if st.pending_cancels.len() > 64 && st.pending_cancels.len() * 2 > st.heap.len() {
            let kept: Vec<Reverse<Entry>> = std::mem::take(&mut st.heap)
                .into_iter()
                .filter(|Reverse(e)| !st.pending_cancels.contains(&e.seq))
                .collect();
            st.heap = BinaryHeap::from(kept);
            st.pending_cancels.clear();
        }
        expired
    });

    // Phase 2: fire wakers without holding the borrow.
    for state in expired {
        state.waker.wake();
    }

    // Phase 3: rearm setTimeout. `schedule` reads the current heap
    // peek, so any timers added by wakers in phase 2 (or by a
    // re-entrant tick) are honoured; we never overwrite a tighter
    // armed setTimeout with a stale deadline.
    schedule();
}

enum Action {
    Stop,
    Fire,
    Cancel,
}

/// Arm `setTimeout` for the next non-cancelled heap entry, clearing
/// any previously-armed timer. Skips heap entries whose seq is in
/// `pending_cancels` (popping them, since they're stale). Runs only
/// on main.
///
/// Reading the current heap (rather than accepting a precomputed
/// deadline) makes us safe against re-entrant ticks: the LAST
/// `schedule` to run always arms the tightest live timer.
fn schedule() {
    MAIN_STATE.with(|m| {
        let mut state = m.borrow_mut();
        let st = state.as_mut().expect("MAIN_STATE initialised");
        let global = js_sys::global();

        if let Some(id) = st.timeout_id.take() {
            if let Some(window) = global.dyn_ref::<web_sys::Window>() {
                window.clear_timeout_with_handle(id);
            } else if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
                scope.clear_timeout_with_handle(id);
            }
        }

        // Find the next live deadline, popping stale (cancelled)
        // entries we encounter at the top.
        let next = loop {
            match st.heap.peek() {
                None => break None,
                Some(Reverse(entry)) => {
                    if st.pending_cancels.contains(&entry.seq) {
                        let popped = st.heap.pop().unwrap().0;
                        st.pending_cancels.remove(&popped.seq);
                        drop(popped);
                        continue;
                    }
                    break Some(entry.deadline_ms);
                }
            }
        };

        let Some(deadline) = next else {
            driver()
                .scheduled_deadline_ms
                .store(i64::MAX, Ordering::Release);
            return;
        };

        let now = now_u64();
        let delay = deadline.saturating_sub(now).min(i32::MAX as u64) as i32;
        let cb = st.callback.as_ref().unchecked_ref();
        let id = if let Some(window) = global.dyn_ref::<web_sys::Window>() {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(cb, delay)
                .expect("setTimeout failed")
        } else if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
            scope
                .set_timeout_with_callback_and_timeout_and_arguments_0(cb, delay)
                .expect("setTimeout failed")
        } else {
            panic!("wasmt::timer: unsupported global scope for setTimeout");
        };
        st.timeout_id = Some(id);
        driver()
            .scheduled_deadline_ms
            .store(deadline as i64, Ordering::Release);
    });
}

/// Submit an insert. If the new deadline is earlier than what is
/// currently armed, kick the driver.
fn submit_insert(deadline_ms: u64, state: Arc<EntryState>) -> u64 {
    let driver = driver();
    let seq = driver.next_seq.fetch_add(1, Ordering::Relaxed);
    driver.ops.push(Op::Insert(Entry {
        deadline_ms,
        seq,
        state,
    }));
    let armed = driver.scheduled_deadline_ms.load(Ordering::Acquire);
    if (deadline_ms as i64) < armed {
        kick();
    }
    seq
}

/// Submit a cancellation. The driver removes the entry on the next
/// tick. We only kick if there might be expired entries pending — a
/// cancellation alone never *needs* to wake the driver, but we keep
/// it cheap by piggybacking when we're already kicking for inserts.
fn submit_cancel(seq: u64) {
    driver().ops.push(Op::Cancel(seq));
    // No kick: cancels resolve lazily on the next scheduled tick.
}

// Per-thread cache for the timer-kick payload. Like
// `cross::post_wake`, we pre-allocate the JS string keys and the
// payload object once per worker — `JsValue::from(&str)` allocates
// a fresh JS string per call, and timer kicks fire on every
// "earlier deadline arrived" event from a worker.
struct KickPostState {
    scope: web_sys::DedicatedWorkerGlobalScope,
    payload: js_sys::Object,
}

thread_local! {
    static KICK_POST_STATE: std::cell::OnceCell<Option<KickPostState>> =
        const { std::cell::OnceCell::new() };
}

fn kick() {
    // Get-or-init the per-thread kick payload. `None` means we're
    // not in a DedicatedWorker scope — fall back to tick() locally.
    KICK_POST_STATE.with(|cell| {
        let st = cell.get_or_init(|| {
            let scope = js_sys::global()
                .dyn_into::<web_sys::DedicatedWorkerGlobalScope>()
                .ok()?;
            let payload = js_sys::Object::new();
            // "kind" is fixed for this payload — initialise once.
            js_sys::Reflect::set(&payload, &"kind".into(), &"wasmt_timer_kick".into()).unwrap();
            Some(KickPostState { scope, payload })
        });
        match st {
            Some(state) => {
                state
                    .scope
                    .post_message(&state.payload)
                    .expect("post_message failed");
            }
            None => tick(),
        }
    });
}

/// JS-callable bridge invoked by main's `onmessage` listener when a
/// worker sends `wasmt_timer_kick`.
#[wasm_bindgen]
pub fn __wasmt_timer_kick() {
    tick();
}

/// A monotonic-ish point in time, in milliseconds since the unix
/// epoch. Comparable across runtime workers and main, since it's
/// derived from `Date.now()`.
///
/// (Browsers don't expose a single monotonic clock that's directly
/// comparable across threads — `performance.now()` has per-context
/// time origins. `Date.now()` advances at wall-clock rate, may go
/// backwards if the system clock is adjusted, but is consistent.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant {
    pub(crate) ms: u64,
}

impl Instant {
    pub fn now() -> Self {
        Instant { ms: now_u64() }
    }

    /// Returns the duration that has elapsed since `earlier`. Saturates
    /// to zero if `earlier` is in the future relative to `self`.
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        let delta = self.ms.saturating_sub(earlier.ms);
        Duration::from_millis(delta)
    }

    pub fn elapsed(&self) -> Duration {
        Instant::now().duration_since(*self)
    }

    pub fn checked_add(&self, dur: Duration) -> Option<Instant> {
        let add_ms: u64 = dur.as_millis().try_into().ok()?;
        Some(Instant {
            ms: self.ms.checked_add(add_ms)?,
        })
    }

    pub fn checked_sub(&self, dur: Duration) -> Option<Instant> {
        let sub_ms: u64 = dur.as_millis().try_into().ok()?;
        Some(Instant {
            ms: self.ms.checked_sub(sub_ms)?,
        })
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, dur: Duration) -> Instant {
        self.checked_add(dur).expect("Instant overflow")
    }
}

impl std::ops::Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, dur: Duration) -> Instant {
        self.checked_sub(dur).expect("Instant underflow")
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, earlier: Instant) -> Duration {
        self.duration_since(earlier)
    }
}

/// Future returned by [`crate::time::sleep`]. `Send + Sync`. Dropping
/// queues a cancellation so the heap entry is freed on the next tick.
pub struct Sleep {
    state: Arc<EntryState>,
    seq: u64,
    deadline: Instant,
    /// `false` once the deadline has fired (so `Drop` skips the
    /// cancel-queue allocation in the common case).
    armed: bool,
}

impl Sleep {
    pub(crate) fn new(dur: Duration) -> Self {
        // `Date.now()` returns integer ms but the real wall-clock
        // it samples is sub-ms; `Date.now() == 1000` can mean any
        // real time in `[1000.0, 1001.0)`. Ceil the duration AND
        // pad by 1 ms so `sleep(50ms)` is guaranteed ≥ 50 ms even
        // measured against a sub-ms clock like `performance.now()`.
        //
        // Saturating arithmetic so a pathological `Duration` near
        // `Duration::MAX` doesn't overflow `u64` and wrap into a
        // tiny (or zero) deadline. With saturation, a request larger
        // than `u64::MAX` ms simply schedules at `u64::MAX` ms —
        // setTimeout will then clamp the delay to `i32::MAX` ms
        // anyway, so the sleep effectively never fires.
        let dur_ms_u128 = dur.as_nanos().div_ceil(1_000_000);
        let dur_ms = u64::try_from(dur_ms_u128).unwrap_or(u64::MAX);
        let deadline = Instant {
            ms: (now_ms() as u64).saturating_add(dur_ms).saturating_add(1),
        };
        Sleep::until(deadline)
    }

    pub(crate) fn until(deadline: Instant) -> Self {
        let state = Arc::new(EntryState {
            elapsed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        });
        let now = Instant::now();
        if deadline <= now {
            // Bypass the driver entirely.
            state.elapsed.store(true, Ordering::Release);
            return Sleep {
                state,
                seq: 0,
                deadline,
                armed: false,
            };
        }
        let seq = submit_insert(deadline.ms, state.clone());
        Sleep {
            state,
            seq,
            deadline,
            armed: true,
        }
    }

    /// The deadline this `Sleep` is waiting for.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// `true` once the deadline has fired (the future has resolved
    /// or will resolve `Ready` on the next poll). Mirrors
    /// `tokio::time::Sleep::is_elapsed`.
    pub fn is_elapsed(&self) -> bool {
        self.state.elapsed.load(Ordering::Acquire)
    }

    /// Reset the sleep to fire at `deadline` (cancels the previous
    /// registration).
    pub fn reset(&mut self, deadline: Instant) {
        if self.armed {
            submit_cancel(self.seq);
        }
        let new_state = Arc::new(EntryState {
            elapsed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        });
        let now = Instant::now();
        if deadline <= now {
            new_state.elapsed.store(true, Ordering::Release);
            self.state = new_state;
            self.seq = 0;
            self.deadline = deadline;
            self.armed = false;
            return;
        }
        let seq = submit_insert(deadline.ms, new_state.clone());
        self.state = new_state;
        self.seq = seq;
        self.deadline = deadline;
        self.armed = true;
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.state.elapsed.load(Ordering::Acquire) {
            self.armed = false;
            return Poll::Ready(());
        }
        self.state.waker.register(cx.waker());
        if self.state.elapsed.load(Ordering::Acquire) {
            self.armed = false;
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if self.armed && !self.state.elapsed.load(Ordering::Acquire) {
            submit_cancel(self.seq);
        }
    }
}

/// Future returned by [`crate::time::timeout`]: resolves to
/// `Ok(output)` if the inner future finishes first, or `Err(Elapsed)`
/// if the timer wins.
pub struct Timeout<F> {
    sleep: Sleep,
    fut: F,
}

impl<F> Timeout<F> {
    pub(crate) fn new(dur: Duration, fut: F) -> Self {
        Timeout {
            sleep: Sleep::new(dur),
            fut,
        }
    }

    pub(crate) fn until(deadline: Instant, fut: F) -> Self {
        Timeout {
            sleep: Sleep::until(deadline),
            fut,
        }
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning of both fields. We never move
        // either out of `self`.
        let this = unsafe { self.get_unchecked_mut() };
        let inner = unsafe { Pin::new_unchecked(&mut this.fut) };
        if let Poll::Ready(v) = inner.poll(cx) {
            return Poll::Ready(Ok(v));
        }
        let sleep = unsafe { Pin::new_unchecked(&mut this.sleep) };
        match sleep.poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Behaviour when an [`Interval`] tick is awaited later than its
/// scheduled deadline. Mirrors `tokio::time::MissedTickBehavior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissedTickBehavior {
    /// **Default.** Missed ticks fire as fast as possible until the
    /// `Interval` catches up. If the awaiter was 3 periods late, the
    /// next 3 `tick()` calls return immediately, then the cadence
    /// resumes from the original schedule.
    #[default]
    Burst,
    /// Missed ticks are coalesced and the cadence shifts so the next
    /// tick is `now + period`. Subsequent ticks fire `period` apart
    /// from there. The original schedule is abandoned.
    Delay,
    /// Missed ticks are dropped; the cadence is preserved (the next
    /// tick fires at the next scheduled deadline that is in the
    /// future, not at one in the past).
    Skip,
}

/// Periodic clock that yields a tick every `period`. Created by
/// [`crate::time::interval`]. Configurable [`MissedTickBehavior`]
/// controls what happens when `tick` is awaited late.
pub struct Interval {
    period: Duration,
    /// Scheduled instant of the NEXT tick. After `tick()` returns,
    /// this is advanced according to the active `MissedTickBehavior`.
    next: Instant,
    sleep: Sleep,
    missed: MissedTickBehavior,
}

impl Interval {
    pub(crate) fn new(period: Duration) -> Self {
        let next = Instant::now() + period;
        Interval {
            period,
            next,
            sleep: Sleep::until(next),
            missed: MissedTickBehavior::default(),
        }
    }

    pub(crate) fn new_at(start: Instant, period: Duration) -> Self {
        Interval {
            period,
            next: start,
            sleep: Sleep::until(start),
            missed: MissedTickBehavior::default(),
        }
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// Current [`MissedTickBehavior`].
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.missed
    }

    /// Set the behaviour for missed ticks. See [`MissedTickBehavior`].
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed = behavior;
    }

    /// Reset the next tick to fire `period` after `Instant::now()`.
    /// Mirrors `tokio::time::Interval::reset`.
    pub fn reset(&mut self) {
        self.next = Instant::now() + self.period;
        self.sleep.reset(self.next);
    }

    /// Reset the next tick to fire at `deadline` (the cadence
    /// continues from there in `period` increments).
    pub fn reset_at(&mut self, deadline: Instant) {
        self.next = deadline;
        self.sleep.reset(deadline);
    }

    /// Wait for the next tick. Returns the [`Instant`] the tick was
    /// scheduled for (which may be earlier than [`Instant::now`] if
    /// the awaiter was slow to poll, in `Burst` mode).
    pub async fn tick(&mut self) -> Instant {
        // `Sleep` is `Unpin`, so we can poll it through a borrow.
        (&mut self.sleep).await;
        let tick_at = self.next;

        let now = Instant::now();
        self.next = match self.missed {
            MissedTickBehavior::Burst => {
                // Original cadence; missed ticks burst-replay because
                // the new `next` may still be in the past, in which
                // case the next `tick().await` returns immediately.
                self.next + self.period
            }
            MissedTickBehavior::Delay => {
                // If we fired on time, preserve original cadence; if
                // late, restart cadence from `now` (matches tokio).
                let candidate = self.next + self.period;
                if candidate < now {
                    now + self.period
                } else {
                    candidate
                }
            }
            MissedTickBehavior::Skip => {
                // Find the next scheduled deadline strictly in the
                // future. `interval()` / `interval_at()` assert that
                // period > 0 at construction, so the divisor here is
                // guaranteed non-zero.
                let mut next = self.next + self.period;
                if next <= now {
                    let behind_ms = now.ms.saturating_sub(next.ms);
                    let period_ms = self.period.as_millis() as u64;
                    // Round up to the next multiple of period.
                    let skips = behind_ms / period_ms + 1;
                    next.ms = next.ms.saturating_add(skips.saturating_mul(period_ms));
                }
                next
            }
        };
        self.sleep.reset(self.next);
        tick_at
    }
}

/// Returned by [`crate::time::timeout`] when the deadline expires
/// before the inner future completes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("timer elapsed")
    }
}

impl std::error::Error for Elapsed {}

#[cfg(test)]
#[doc(hidden)]
pub fn pending_entries() -> usize {
    MAIN_STATE.with(|m| m.borrow().as_ref().map(|s| s.heap.len()).unwrap_or(0))
}
