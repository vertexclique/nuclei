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
            Driver::Ring(_) => crate::IoBackend::IoUring,
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

    /// Publishes the polls, the remote wakes, the timers, the buffer pool
    /// and the driver's operations.
    pub(super) fn publish(&self) {
        let (ops_in_flight, cq_overflows) = match &self.driver {
            #[cfg(ring)]
            Driver::Ring(ring) => (ring.in_flight(), ring.overflows()),
            // A readiness poller hands the kernel no operation.
            #[cfg(readiness)]
            Driver::Poll(_) => (0, 0),
            #[cfg(port)]
            Driver::Poll(_) => (self.shared.port.out(), 0),
        };
        self.shared.counters.publish(Published {
            polls: self.polls.get(),
            remote_wakes: self.remote_wakes.get(),
            timers: self.wheel.borrow().armed(),
            buffer_bytes: self.pool.charged(),
            buffer_exhausted: self.pool.exhausted(),
            ops_in_flight,
            cq_overflows,
        });
    }
}
