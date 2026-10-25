//! A shard that stops running: for now (the outermost `block_on` of its
//! thread returned) or for good (dropped). What it watches goes, each to be
//! watched where its user polls next: the sources, each told; the
//! children, each once its registration ended. A ring waits for the
//! operations still out to end; a port hands them to a keeper.

use core::sync::atomic::Ordering;
use std::sync::Arc;

#[cfg(ring)]
use super::Driver;
use super::Shard;
use crate::io::Source;

impl Shard {
    /// The outermost `block_on` of a thread outside the runtime returned,
    /// and this, its own shard, runs again only at its next one.
    pub(crate) fn leave(&self) {
        let gone: Vec<(u64, Arc<Source>)> = self.sources.borrow_mut().drain_tokens().collect();
        evict_all(gone);
        self.let_go_of_children();
        // Sources that went before (a dropped handle's) may have left a
        // cancel queued, or their operations still out.
        #[cfg(ring)]
        if let Driver::Ring(ring) = &self.driver {
            ring.settle(|| self.apply_io_ops());
        }
        // The port's go to its keeper.
        #[cfg(port)]
        if !self.hand_off_port() {
            self.settle_port();
        }
    }
}

/// Evicts each source of `gone` (registered under its token here), and
/// waits until each one's registration here has gone. A source a side of
/// which another thread holds is evicted by that holder as it lets go,
/// which comes a driver call later; until then the holder may still hand
/// this shard's driver an operation, so the operations out here are only
/// known once every eviction happened.
fn evict_all(gone: Vec<(u64, Arc<Source>)>) {
    for (_, source) in &gone {
        source.evict();
    }
    for (token, source) in &gone {
        while source.registered_under(*token) {
            std::thread::yield_now();
        }
    }
}

impl Drop for Shard {
    fn drop(&mut self) {
        self.shared.counters.went();
        // Every task still queued here is dropped on this thread, the one its
        // future may be dropped on.
        self.shared.draining.store(true, Ordering::SeqCst);
        self.shared.stopped.store(true, Ordering::SeqCst);
        // The waker a worker's loop leaves when its look after registering
        // finds it may exit: it holds the shard's `Shared`, which holds it.
        drop(self.shared.stop_waker.take());
        // The sources registered here lose their driver: each is told, and
        // registers where it is polled next.
        let gone: Vec<(u64, Arc<Source>)> = self.sources.get_mut().drain_tokens().collect();
        evict_all(gone);
        self.let_go_of_children();
        // The ring's operations still out are cancelled and their
        // completions waited for, so no memory they point at goes while the
        // kernel may write it.
        #[cfg(ring)]
        if let Driver::Ring(ring) = &self.driver {
            // Cancels a holder of a source on another thread left here.
            ring.settle(|| self.apply_io_ops());
            ring.shut_down();
        }
        // The port's operations to a keeper, unless one has them already.
        #[cfg(port)]
        if !self.port_kept() && !self.hand_off_port() {
            self.settle_port();
        }
        // What goes with a task may wake another of the shard's (a timer's
        // waker, a channel its future held): that one comes back through the
        // inbox, and is dropped here too, until none is left.
        loop {
            let lifo = self.lifo.take();
            let queue = core::mem::take(&mut *self.queue.borrow_mut());
            let timers = self.wheel.borrow_mut().take_wakers();
            let fired = core::mem::take(&mut *self.fired.borrow_mut());
            let mut quiet =
                lifo.is_none() && queue.is_empty() && timers.is_empty() && fired.is_empty();
            drop((lifo, queue, timers, fired));
            while let Some(runnable) = self.pop_inbox() {
                drop(runnable);
                quiet = false;
            }
            if quiet {
                break;
            }
        }
    }
}
