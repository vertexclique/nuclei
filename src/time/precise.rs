//! [`SleepPrecise`]: a sleep that ends at its deadline to the microsecond,
//! off the millisecond wheel.
//!
//! A precise timer is armed in its shard's microsecond wheel (the same wheel
//! as the millisecond one, counting microseconds), and the shard's wait ends
//! at the earliest deadline of either wheel through the backend's
//! high-resolution timer: the wait's own timeout on io_uring (its hrtimer has
//! no slack) and on Windows (a high-resolution waitable timer), a timerfd on
//! epoll (whose own timeout counts milliseconds), and an `EVFILT_TIMER` in
//! nanoseconds on kqueue. A paused clock moves to a precise deadline exactly.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::time::Duration;

use super::Instant;
use super::sleep::{Fine, Timer, far_future};

/// Waits until `duration` has passed, to the microsecond.
///
/// The wait is at least `duration`, rounded up to the microsecond, and ends
/// as soon after that as the system's timer wakes the shard; a busy shard
/// may let it run later by the time its current task takes to yield. Use it
/// where the millisecond of [`sleep`](super::sleep) is too coarse: a pacer, a
/// retry spaced in microseconds, a deadline a protocol counts in
/// microseconds. Each precise deadline that is the shard's earliest costs
/// the shard a timer of its own in the kernel (one system call to arm it on
/// epoll), so a coarse sleep stays the cheaper choice where a millisecond is
/// fine.
///
/// ```
/// use std::time::Duration;
/// use nuclei::time::{self, Instant};
///
/// # nuclei::Builder::new().worker_threads(0).build().unwrap().block_on(async {
/// time::pause();
/// let start = Instant::now();
/// time::sleep_precise(Duration::from_micros(250)).await;
/// // A paused clock moves to the deadline itself: not a millisecond later.
/// assert_eq!(start.elapsed(), Duration::from_micros(250));
/// # });
/// ```
pub fn sleep_precise(duration: Duration) -> SleepPrecise {
    sleep_until_precise(
        Instant::now()
            .checked_add(duration)
            .unwrap_or_else(far_future),
    )
}

/// Waits until `deadline`, to the microsecond (see [`sleep_precise`]).
pub fn sleep_until_precise(deadline: impl Into<Instant>) -> SleepPrecise {
    SleepPrecise {
        timer: Timer::new(deadline.into()),
    }
}

/// A future that completes at its deadline, to the microsecond. Made by
/// [`sleep_precise`] and [`sleep_until_precise`].
///
/// Polled on one of nuclei's shards, it arms a timer in that shard's
/// microsecond wheel; polled anywhere else (another runtime's thread), it
/// asks a shard of the current runtime (or the default one) to fire it.
/// Dropping it cancels the timer.
#[must_use = "a sleep does nothing unless awaited"]
pub struct SleepPrecise {
    timer: Timer<Fine>,
}

impl SleepPrecise {
    /// The deadline.
    pub fn deadline(&self) -> Instant {
        self.timer.deadline
    }

    /// Whether the deadline has passed.
    pub fn is_elapsed(&self) -> bool {
        self.timer.is_elapsed()
    }

    /// Moves the deadline; the sleep is pending again until it.
    pub fn reset(self: Pin<&mut Self>, deadline: impl Into<Instant>) {
        self.get_mut().timer.reset(deadline.into());
    }
}

impl Future for SleepPrecise {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.get_mut().timer.poll(cx)
    }
}

impl fmt::Debug for SleepPrecise {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SleepPrecise")
            .field("deadline", &self.deadline())
            .field("elapsed", &self.is_elapsed())
            .finish()
    }
}
