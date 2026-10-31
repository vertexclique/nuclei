//! What a shard publishes of its own cells for `stats`, each time it looks
//! at its driver.

use super::{Driver, Shard};
use crate::rt::counters::Published;

impl Driver {
    /// The kernel interface the driver runs IO on.
    pub(super) fn backend(&self) -> crate::IoBackend {
        match self {
            Driver::Poll(_) => crate::proactor::POLLER,
            #[cfg(ring)]
            Driver::Ring(_) | Driver::Gone => crate::IoBackend::IoUring,
        }
    }
}

impl Shard {
    /// Publishes at once what the shard holds, its run loop's polls
    /// counted in: what a test reads without waiting for the shard to look.
    #[cfg(test)]
    pub(crate) fn publish_now(&self) {
        self.count_again(self.since_drive.get());
        self.publish();
    }

    /// Counts a sleep of `slept` for `stats` and tells the spin how long it
    /// was.
    pub(super) fn slept_for(&self, slept: std::time::Duration) {
        self.sleeps.set(self.sleeps.get() + 1);
        self.slept.set(self.slept.get() + slept);
        self.spin.parked_for(slept);
    }

    /// Publishes the polls, the sleeps, the queue, the remote wakes, the
    /// timers, the buffer pool and the driver's operations.
    pub(super) fn publish(&self) {
        let (ops_in_flight, cq_overflows) = match &self.driver {
            #[cfg(ring)]
            Driver::Ring(ring) => (ring.in_flight(), ring.overflows()),
            #[cfg(ring)]
            Driver::Gone => (0, 0),
            // A readiness poller hands the kernel no operation.
            #[cfg(readiness)]
            Driver::Poll(_) => (0, 0),
            #[cfg(port)]
            Driver::Poll(_) => (self.shared.port.out(), 0),
        };
        self.shared.counters.publish(Published {
            polls: self.polls.get(),
            parks: self.sleeps.get(),
            parked: self.slept.get(),
            queue_depth: self.queued(),
            remote_wakes: self.remote_wakes.get(),
            timers: self.wheel.borrow().armed(),
            buffer_bytes: self.pool.charged(),
            buffer_exhausted: self.pool.exhausted(),
            ops_in_flight,
            cq_overflows,
        });
    }
}
