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
        self.count_again(self.since_drive());
        self.publish();
    }

    #[cfg(all(completion, feature = "io-stats"))]
    /// Has the shard's ring or port stamp the operations it issues from now
    /// on, to record their latencies; a readiness poller issues none.
    pub(super) fn time_io(&self) {
        let Some(times) = self.shared.counters.io_times() else {
            return;
        };
        match &self.driver {
            #[cfg(ring)]
            Driver::Ring(ring) => ring.time_with(std::sync::Arc::clone(times)),
            #[cfg(ring)]
            Driver::Gone => {}
            #[cfg(readiness)]
            Driver::Poll(_) => {}
            #[cfg(port)]
            Driver::Poll(_) => self.shared.port.time_with(std::sync::Arc::clone(times)),
        }
    }

    /// Counts a sleep of `slept` nanoseconds for `stats` and, unless a blocking
    /// job was out (`tells_the_spin` false), tells the spin how long it was.
    #[inline]
    pub(super) fn slept_for(&self, slept: u64, tells_the_spin: bool) {
        self.sleeps.set(self.sleeps.get() + 1);
        self.slept.set(self.slept.get().saturating_add(slept));
        if tells_the_spin {
            self.spin.parked_for(slept);
        }
    }

    /// The shard parks, from `slept` on (a `system_nanos` reading): `stats`
    /// counts the park as it goes.
    #[inline]
    pub(super) fn park_began(&self, slept: u64) {
        let counters = &self.shared.counters;
        counters.park_began(slept.saturating_sub(counters.born()), self.slept.get());
    }

    /// The shard woke, its park counted by `slept_for`.
    #[inline]
    pub(super) fn park_ended(&self) {
        self.shared.counters.park_ended(self.slept.get());
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
