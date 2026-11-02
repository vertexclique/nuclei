//! Time: sleeping, timeouts and intervals, on the timer wheel of the shard
//! that polls them, and the runtime's clock they read.
//!
//! The clock is [`Instant`](crate::time::Instant). It runs with the system's monotonic clock until
//! [`pause`](crate::time::pause) stops it (or a runtime built with
//! [`start_paused`](crate::Builder::start_paused) starts stopped); a paused
//! clock moves only by [`advance`](crate::time::advance), or by itself when every shard of its
//! runtime is idle with a timer armed, no blocking job runs and no poll the
//! handoff took from its worker still runs: it then jumps to the earliest
//! timer, so code that sleeps runs at once in tests. Timers fire on the wheel's
//! millisecond: a timer waits at least its duration, rounded up to the next
//! millisecond, and a shard busy with a task fires it once that task yields.
//! [`sleep_precise`](crate::time::sleep_precise) waits to the microsecond
//! instead, off the millisecond wheel: its deadline ends the shard's wait
//! through the backend's high-resolution timer, and a paused clock moves to
//! it exactly; `sleep` and `timeout` on the wheel pay nothing for it.
//! Arming and cancelling a timer on its shard costs no allocation once the
//! shard's wheel has grown to the most timers alive at once; a timer polled on
//! a thread that is not a shard (another runtime's) is fired for it by a
//! shard of the current runtime, or of the default one.

pub(crate) mod clock;
pub mod error;
mod instant;
mod interval;
#[cfg(all(test, not(loom)))]
mod nanos_tests;
mod precise;
mod sleep;
mod timeout;
pub(crate) mod units;
pub(crate) mod wheel;

pub use self::instant::Instant;
pub use self::interval::{Interval, MissedTickBehavior, interval, interval_at};
pub use self::precise::{SleepPrecise, sleep_precise, sleep_until_precise};
pub(crate) use self::sleep::TimerOp;
pub use self::sleep::{Sleep, sleep, sleep_until};
pub use self::timeout::{Timeout, timeout, timeout_at};

use core::time::Duration;

/// Stops the clock of the runtime this code runs in (or of the default
/// runtime): [`Instant::now`] stands still, and timers fire only as the clock
/// is moved, by [`advance`] or by itself whenever every shard of the runtime is
/// idle with a timer armed, no blocking job runs and no poll the handoff took
/// from its worker still runs (it jumps to the earliest timer). A paused clock
/// stays paused, and never reads earlier than a time it gave before the pause.
/// A nested `block_on` of another runtime on the same thread keeps its own
/// clock and timers, and its paused clock does not count that shard.
///
/// The shards counted are the runtime's workers and the shards of the threads
/// that entered its `block_on` while the clock was already paused. A thread
/// inside `block_on` from before a pause made elsewhere (in a spawned task on a
/// worker, say) is not counted: a sleep it waits on is no jump target, and
/// waits until something else wakes the thread, which may be never. Pause in
/// the `block_on` future itself, before it, or with
/// [`start_paused`](crate::Builder::start_paused).
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
