//! Time: sleeping, timeouts and intervals, on the timer wheel of the shard
//! that polls them.
//!
//! The clock is [`std::time::Instant`], re-exported here as `Instant`.
//! Timers fire on the wheel's millisecond: a timer waits at least its
//! duration, rounded up to the next millisecond, and a shard busy with a task
//! fires it once that task yields. Arming and cancelling a timer on its shard
//! costs no allocation once the shard's wheel has grown to the most timers
//! alive at once; a timer polled on a thread that is not a shard (another
//! runtime's) is fired for it by a shard of the current runtime, or of the
//! default one.

pub mod error;
mod interval;
mod sleep;
mod timeout;
pub(crate) mod wheel;

pub use std::time::Instant;

pub use self::interval::{Interval, MissedTickBehavior, interval, interval_at};
pub(crate) use self::sleep::TimerOp;
pub use self::sleep::{Sleep, sleep, sleep_until};
pub use self::timeout::{Timeout, timeout, timeout_at};

#[cfg(test)]
mod tests;
