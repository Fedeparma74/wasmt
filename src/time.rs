//! Time primitives.
//!
//! [`sleep`] returns a `Send` future that yields the current task
//! until the deadline elapses; it is driven by a single shared timer
//! managed on the main thread, so there is no per-sleep `setTimeout`
//! Closure and any number of concurrent sleeps cost only one armed
//! timeout at any moment.
//!
//! [`timeout`] races a future against a [`sleep`] of the same shape.
//!
//! [`sleep_blocking`] parks the current thread for the duration. It
//! must only be called from a worker scope (a runtime worker, a
//! [`crate::spawn_blocking`] worker, or any other Web Worker) — wasm
//! `memory.atomic.wait32` throws on the main thread.

use std::time::Duration;

pub use crate::runtime::timer::{Elapsed, Instant, Interval, MissedTickBehavior, Sleep, Timeout};

/// Wait for the given duration. The returned future is `Send`, so it
/// can be `.await`-ed inside any [`crate::spawn`] or
/// [`crate::spawn_local`] task.
pub fn sleep(dur: Duration) -> Sleep {
    Sleep::new(dur)
}

/// Wait until the given [`Instant`]. If the deadline is already in
/// the past the returned `Sleep` resolves on the first poll.
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep::until(deadline)
}

/// Race a future against a deadline. Returns `Ok(output)` if the
/// future completes first; `Err(`[`Elapsed`]`)` if the timer wins.
pub fn timeout<F>(dur: Duration, fut: F) -> Timeout<F>
where
    F: std::future::Future,
{
    Timeout::new(dur, fut)
}

/// Race a future against an absolute deadline. Returns `Ok(output)`
/// if the future completes before `deadline`; `Err(`[`Elapsed`]`)`
/// otherwise.
pub fn timeout_at<F>(deadline: Instant, fut: F) -> Timeout<F>
where
    F: std::future::Future,
{
    Timeout::until(deadline, fut)
}

/// Returns a clock that yields a tick every `period`. The first tick
/// fires after `period` has elapsed.
pub fn interval(period: Duration) -> Interval {
    assert!(!period.is_zero(), "interval period must be non-zero");
    Interval::new(period)
}

/// Returns a clock that yields its first tick at `start`, then every
/// `period` thereafter.
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(!period.is_zero(), "interval period must be non-zero");
    Interval::new_at(start, period)
}

/// Park the current thread for `dur` (maps to `std::thread::sleep` →
/// `Atomics.wait`).
///
/// **Cannot be called on the main thread.**
pub fn sleep_blocking(dur: Duration) {
    std::thread::sleep(dur);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_test::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(thread_local_v2, js_name = "performance")]
        pub static PERFORMANCE: web_sys::Performance;
    }

    wasm_bindgen_test_configure!(run_in_browser);

    fn now() -> f64 {
        PERFORMANCE.with(|p| p.now())
    }

    #[wasm_bindgen_test]
    async fn sleep_resolves_after_duration() {
        let start = now();
        sleep(Duration::from_millis(50)).await;
        let elapsed = now() - start;
        assert!(elapsed >= 50.0, "elapsed = {elapsed}ms");
    }

    #[wasm_bindgen_test]
    async fn sleep_zero_returns_promptly() {
        let start = now();
        sleep(Duration::ZERO).await;
        assert!(now() - start < 200.0);
    }

    #[wasm_bindgen_test]
    async fn sleep_is_send_and_runs_inside_spawn() {
        let start = now();
        let h = crate::spawn(async {
            sleep(Duration::from_millis(40)).await;
            42u32
        });
        assert_eq!(h.join().await.unwrap(), 42);
        let elapsed = now() - start;
        assert!(elapsed >= 40.0, "elapsed = {elapsed}ms");
    }

    #[wasm_bindgen_test]
    async fn many_concurrent_sleeps_all_complete() {
        // A big batch sharing the single armed setTimeout.
        let mut handles = Vec::with_capacity(40);
        for i in 0..40u32 {
            handles.push(crate::spawn(async move {
                sleep(Duration::from_millis(10 + (i % 5) as u64 * 5)).await;
                i
            }));
        }
        let mut total = 0u32;
        for h in handles {
            total += h.join().await.unwrap();
        }
        assert_eq!(total, (0..40u32).sum::<u32>());
    }

    #[wasm_bindgen_test]
    async fn drop_sleep_cancels_pending_entry() {
        let s = sleep(Duration::from_secs(60));
        drop(s);
        sleep(Duration::from_millis(20)).await;
    }

    #[wasm_bindgen_test]
    async fn instant_now_advances_monotonically() {
        let a = super::Instant::now();
        sleep(Duration::from_millis(10)).await;
        let b = super::Instant::now();
        assert!(b >= a);
        let elapsed = b.duration_since(a);
        assert!(elapsed >= Duration::from_millis(8));
    }

    #[wasm_bindgen_test]
    async fn sleep_until_resolves_at_deadline() {
        let deadline = super::Instant::now() + Duration::from_millis(40);
        sleep_until(deadline).await;
        assert!(super::Instant::now() >= deadline);
    }

    #[wasm_bindgen_test]
    async fn sleep_until_in_the_past_returns_immediately() {
        let past = super::Instant::now() - Duration::from_secs(1);
        let start = now();
        sleep_until(past).await;
        assert!(now() - start < 50.0);
    }

    #[wasm_bindgen_test]
    async fn timeout_at_resolves_or_fires() {
        let deadline = super::Instant::now() + Duration::from_millis(20);
        let r = timeout_at(deadline, async { 1u32 }).await;
        assert_eq!(r, Ok(1));

        let deadline = super::Instant::now() + Duration::from_millis(20);
        let r = timeout_at(deadline, async {
            sleep(Duration::from_secs(60)).await;
            0u32
        })
        .await;
        assert!(matches!(r, Err(Elapsed)));
    }

    #[wasm_bindgen_test]
    async fn interval_ticks_periodically() {
        let period = Duration::from_millis(20);
        let mut tk = interval(period);
        let start = super::Instant::now();
        const N: u32 = 3;
        for _ in 0..N {
            tk.tick().await;
        }
        let elapsed = super::Instant::now().duration_since(start);
        // Lower bound: roughly 2 periods (the first tick fires at
        // ~`period` after start, then 2 more spaced by `period`).
        // Accept a small slack for Date.now() coarseness.
        let min = period.saturating_mul(N - 1);
        assert!(
            elapsed >= min.saturating_sub(Duration::from_millis(5)),
            "elapsed too short: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "elapsed too long: {elapsed:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn interval_at_starts_at_specified_instant() {
        let start = super::Instant::now() + Duration::from_millis(20);
        let mut tk = interval_at(start, Duration::from_millis(10));
        let first = tk.tick().await;
        assert!(first >= start);
    }

    #[wasm_bindgen_test]
    async fn timeout_resolves_on_inner_completion() {
        let r = timeout(Duration::from_millis(200), async { 7u32 }).await;
        assert_eq!(r, Ok(7));
    }

    #[wasm_bindgen_test]
    async fn timeout_fires_on_deadline() {
        let r = timeout(Duration::from_millis(20), async {
            sleep(Duration::from_secs(60)).await;
            0u32
        })
        .await;
        assert!(matches!(r, Err(Elapsed)));
    }

    #[wasm_bindgen_test]
    async fn sleep_blocking_in_spawn_blocking() {
        let h = crate::spawn_blocking(|| {
            let start = now();
            sleep_blocking(Duration::from_millis(50));
            now() - start
        });
        assert!(h.join().await.unwrap() >= 50.0);
    }

    #[wasm_bindgen_test]
    async fn sleep_blocking_only_blocks_one_worker() {
        // A blocking sleep on a `spawn_blocking` worker must not stall
        // pool-async work running concurrently.
        let blocking = crate::spawn_blocking(|| {
            sleep_blocking(Duration::from_millis(120));
            7u32
        });
        let async_h = crate::spawn(async {
            sleep_blocking(Duration::from_millis(20));
            8u32
        });
        let start = now();
        let (a, b) = (
            async_h.join().await.unwrap(),
            blocking.join().await.unwrap(),
        );
        let elapsed = now() - start;
        assert_eq!((a, b), (8, 7));
        assert!(elapsed < 250.0, "elapsed too long: {elapsed}");
    }
}
