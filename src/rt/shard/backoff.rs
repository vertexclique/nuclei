//! The pause between two looks of a bounded wait on another thread: a few yields (the common
//! case is microseconds), then sleeps that double up to a millisecond.

use std::time::Duration;

/// Looks that only yield before the wait sleeps.
const YIELDS: u32 = 16;
/// The first sleep between looks, doubling to [`PAUSE_MAX`].
const PAUSE_FIRST: Duration = Duration::from_micros(50);
/// The longest sleep between two looks.
const PAUSE_MAX: Duration = Duration::from_millis(1);

/// The state of one wait's pauses.
pub(crate) struct Backoff {
    looks: u32,
    pause: Duration,
}

impl Backoff {
    pub(crate) const fn new() -> Self {
        Self {
            looks: 0,
            pause: PAUSE_FIRST,
        }
    }

    /// Whether the next pause only yields.
    pub(crate) const fn yielding(&self) -> bool {
        self.looks < YIELDS
    }

    /// Pauses before the next look.
    pub(crate) fn pause(&mut self) {
        if self.yielding() {
            self.looks += 1;
            std::thread::yield_now();
        } else {
            std::thread::sleep(self.pause);
            self.pause = (self.pause * 2).min(PAUSE_MAX);
        }
    }
}
