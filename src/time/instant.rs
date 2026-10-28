//! [`Instant`]: a point on the runtime's clock, which can be paused and
//! advanced (see [`pause`](super::pause)); otherwise std's `Instant`.

use core::fmt;
use core::ops::{Add, AddAssign, Sub, SubAssign};
use std::time::Duration;

/// A point in time on the clock of the runtime the code runs in.
///
/// It is std's [`Instant`](std::time::Instant) as long as the runtime's clock
/// runs as the system's; [`pause`](super::pause) stops it and
/// [`advance`](super::advance) moves it, and every timer follows it. The two
/// convert both ways ([`from_std`](Self::from_std), [`into_std`](Self::into_std)).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(std::time::Instant);

impl Instant {
    /// The time now on the runtime's clock.
    pub fn now() -> Self {
        Self(super::clock::now())
    }

    /// The instant `std` names.
    pub fn from_std(std: std::time::Instant) -> Self {
        Self(std)
    }

    /// This instant as std's.
    pub fn into_std(self) -> std::time::Instant {
        self.0
    }

    /// The time from `earlier` to this instant, zero if `earlier` is later.
    pub fn duration_since(&self, earlier: Self) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }

    /// The time from `earlier` to this instant, `None` if `earlier` is later.
    pub fn checked_duration_since(&self, earlier: Self) -> Option<Duration> {
        self.0.checked_duration_since(earlier.0)
    }

    /// The time from `earlier` to this instant, zero if `earlier` is later.
    pub fn saturating_duration_since(&self, earlier: Self) -> Duration {
        self.0.saturating_duration_since(earlier.0)
    }

    /// The time on the runtime's clock since this instant.
    pub fn elapsed(&self) -> Duration {
        Self::now().saturating_duration_since(*self)
    }

    /// This instant moved forward by `duration`, `None` past the clock's range.
    pub fn checked_add(&self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }

    /// This instant moved back by `duration`, `None` before the clock's range.
    pub fn checked_sub(&self, duration: Duration) -> Option<Self> {
        self.0.checked_sub(duration).map(Self)
    }
}

impl From<std::time::Instant> for Instant {
    fn from(std: std::time::Instant) -> Self {
        Self(std)
    }
}

impl From<Instant> for std::time::Instant {
    fn from(instant: Instant) -> Self {
        instant.0
    }
}

impl Add<Duration> for Instant {
    type Output = Self;

    /// # Panics
    ///
    /// Past the clock's range; [`checked_add`](Instant::checked_add) does not.
    fn add(self, duration: Duration) -> Self {
        Self(self.0 + duration)
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, duration: Duration) {
        self.0 += duration;
    }
}

impl Sub<Duration> for Instant {
    type Output = Self;

    /// # Panics
    ///
    /// Before the clock's range; [`checked_sub`](Instant::checked_sub) does not.
    fn sub(self, duration: Duration) -> Self {
        Self(self.0 - duration)
    }
}

impl SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, duration: Duration) {
        self.0 -= duration;
    }
}

impl Sub for Instant {
    type Output = Duration;

    /// The time from `earlier` to `self`, zero if `earlier` is later.
    fn sub(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

impl fmt::Debug for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
