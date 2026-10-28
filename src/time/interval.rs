//! [`Interval`]: ticks at a fixed period.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::time::Duration;

use super::Instant;

use super::sleep::{Sleep, sleep_until};

/// What an interval does with ticks it missed because it was polled late.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MissedTickBehavior {
    /// Every missed tick fires as soon as it can, one after the other, until
    /// the interval is back on its schedule.
    #[default]
    Burst,
    /// The next tick comes one period after the late one: the schedule moves.
    Delay,
    /// Missed ticks are skipped: the next tick is the next one on the
    /// original schedule after now.
    Skip,
}

/// Ticks every `period`, the first tick at once.
///
/// # Panics
///
/// If `period` is zero.
pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now(), period)
}

/// Ticks every `period`, the first tick at `start`.
///
/// # Panics
///
/// If `period` is zero.
pub fn interval_at(start: impl Into<Instant>, period: Duration) -> Interval {
    let start = start.into();
    assert!(
        period > Duration::ZERO,
        "an interval needs a period above zero"
    );
    Interval {
        period,
        sleep: sleep_until(start),
        missed: MissedTickBehavior::default(),
    }
}

/// Ticks at a fixed period. Made by [`interval`] and [`interval_at`].
#[derive(Debug)]
pub struct Interval {
    period: Duration,
    /// Until the next tick.
    sleep: Sleep,
    missed: MissedTickBehavior,
}

impl Interval {
    /// Waits for the next tick and returns the instant it was due at.
    pub async fn tick(&mut self) -> Instant {
        poll_fn(|cx| self.poll_tick(cx)).await
    }

    /// Polls for the next tick; see [`tick`](Self::tick).
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        if Pin::new(&mut self.sleep).poll(cx).is_pending() {
            return Poll::Pending;
        }
        let due = self.sleep.deadline();
        let now = Instant::now();
        let next = match self.missed {
            MissedTickBehavior::Burst => due + self.period,
            MissedTickBehavior::Delay => now + self.period,
            MissedTickBehavior::Skip => {
                let late = now.saturating_duration_since(due);
                // The next deadline on the schedule after now.
                let periods = late.as_nanos() / self.period.as_nanos() + 1;
                due + self.period * u32::try_from(periods).unwrap_or(u32::MAX)
            }
        };
        Pin::new(&mut self.sleep).reset(next);
        Poll::Ready(due)
    }

    /// Restarts the schedule: the next tick is one period from now.
    pub fn reset(&mut self) {
        Pin::new(&mut self.sleep).reset(Instant::now() + self.period);
    }

    /// The period.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// What happens to missed ticks.
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.missed
    }

    /// Sets what happens to missed ticks.
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed = behavior;
    }
}
