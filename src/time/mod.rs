//! Time: sleeping, timeouts and intervals, on the timer wheel of the shard
//! that polls them, and the runtime's clock they read.
//!
//! The clock is [`Instant`](crate::time::Instant). It runs with the system's monotonic clock until
//! [`pause`](crate::time::pause) stops it (or a runtime built with
//! [`start_paused`](crate::Builder::start_paused) starts stopped); a paused
//! clock moves only by [`advance`](crate::time::advance), or by itself when every shard of its
//! runtime is idle with a timer armed: it then jumps to the earliest timer, so
//! code that sleeps runs at once in tests. Timers fire on the wheel's
//! millisecond: a timer waits at least its duration, rounded up to the next
//! millisecond, and a shard busy with a task fires it once that task yields.
//! Arming and cancelling a timer on its shard costs no allocation once the
//! shard's wheel has grown to the most timers alive at once; a timer polled on
//! a thread that is not a shard (another runtime's) is fired for it by a
//! shard of the current runtime, or of the default one.

pub(crate) mod clock;
pub mod error;
mod instant;
mod interval;
mod sleep;
mod timeout;
pub(crate) mod wheel;

pub use self::instant::Instant;
pub use self::interval::{Interval, MissedTickBehavior, interval, interval_at};
pub(crate) use self::sleep::TimerOp;
pub use self::sleep::{Sleep, sleep, sleep_until};
pub use self::timeout::{Timeout, timeout, timeout_at};

use core::time::Duration;

/// Stops the clock of the runtime this code runs in (or of the default
/// runtime): [`Instant::now`] stands still, and timers fire only as the clock
/// is moved, by [`advance`] or by itself whenever every shard of the runtime is
/// idle with a timer armed (it jumps to the earliest one). A paused clock stays
/// paused.
///
/// ```
/// use std::time::Duration;
/// use nuclei::time::{self, Instant};
///
/// # nuclei::Builder::new().worker_threads(0).build().unwrap().block_on(async {
/// time::pause();
/// let start = Instant::now();
/// // Nothing else runs: the clock jumps to the sleep's end at once.
/// time::sleep(Duration::from_secs(3600)).await;
/// assert_eq!(Instant::now() - start, Duration::from_secs(3600));
/// # });
/// ```
pub fn pause() {
    crate::rt::paused::pause();
}

/// Starts a paused clock again from the time it stands at; a running clock
/// goes on as it is.
pub fn resume() {
    crate::rt::paused::resume();
}

/// Moves the clock of the runtime this code runs in forward by `duration`,
/// paused or running, then lets the timers that came due fire before it
/// returns.
///
/// ```
/// use std::time::Duration;
/// use nuclei::time::{self, Instant};
///
/// # nuclei::Builder::new().worker_threads(0).build().unwrap().block_on(async {
/// time::pause();
/// let start = Instant::now();
/// time::advance(Duration::from_millis(250)).await;
/// assert_eq!(start.elapsed(), Duration::from_millis(250));
/// # });
/// ```
pub async fn advance(duration: Duration) {
    crate::rt::paused::advance(duration);
    crate::task::yield_now().await;
}

#[cfg(test)]
mod tests;
