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
    /// Runs `future` to its end on this shard, a thread's own, entered for
    /// the outermost `block_on` of the thread; then leaves, whether the
    /// call returned or unwound, so what waits on this shard is handed on.
    pub(crate) fn block_on_then_leave<F: core::future::Future>(&self, future: F) -> F::Output {
        let mut future = core::pin::pin!(future);
        crate::rt::context::deeper(self.limits.max_nesting, || {
            let signal = Arc::new(super::signal::Signal::new(&self.shared));
            self.run_then_leave(future.as_mut(), &signal)
        })
    }

    /// The outermost `block_on` of a thread outside the runtime returned,
    /// and this, its own shard, runs again only at its next one.
    pub(crate) fn leave(&self) {
        let gone: Vec<(u64, Arc<Source>)> = self.sources.borrow_mut().drain_tokens().collect();
        evict_all(gone, self.shared_addr());
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

/// Evicts each source of `gone` (registered under its token with the shard
/// at `home`, this one), and waits until each one's registration here has
/// gone. A source a side of which another thread holds is evicted by that
/// holder as it lets go, which comes a driver call later; until then the
/// holder may still hand this shard's driver an operation, so the
/// operations out here are only known once every eviction happened.
fn evict_all(gone: Vec<(u64, Arc<Source>)>, home: usize) {
    for (_, source) in &gone {
        source.evict();
    }
    wait_for_evictions(&gone, home);
}

/// Waits until no source of `gone` is registered under its token with the
/// shard at `home`. A source evicted from here may register with another
/// shard at once, under a token of the same number: that registration is
/// not waited for, or this shard would wait for as long as it lasts, with
/// the eviction's cancel of the source's operations still in its mailbox.
fn wait_for_evictions(gone: &[(u64, Arc<Source>)], home: usize) {
    for (token, source) in gone {
        while source.registered_with(home, *token) {
            std::thread::yield_now();
        }
    }
}

#[cfg(all(test, unix, not(loom)))]
mod tests {
    use core::future::{Future, poll_fn};
    use core::task::Poll;
    use std::sync::Arc;
    use std::time::Duration;

    /// A shard that goes waits for its own registrations of its sources
    /// only: a source evicted from it that registered with another shard
    /// since, under a token of the same number (each shard numbers its
    /// own), is not waited for.
    #[test]
    fn a_shard_that_goes_does_not_wait_for_a_registration_elsewhere_under_its_token() {
        let (mine, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let handle = crate::io::Handle::new(mine).unwrap();
        let src = Arc::clone(handle.source());
        crate::block_on(async {
            // A receive polled here registers the source with this thread's
            // shard.
            let mut buf = [0u8; 1];
            {
                let mut read = Box::pin(handle.recv(&mut buf));
                let polled = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                assert!(polled.is_pending());
            }
            let (home, token) = src.registration();
            assert_ne!(home, 0, "the receive did not register the source");
            // Another shard evicted the source under the same token number,
            // and waits for that eviction.
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let other = {
                let src = Arc::clone(&src);
                std::thread::spawn(move || {
                    crate::block_on(async move {
                        let there =
                            crate::rt::context::current(|s| s.map(super::Shard::shared_addr))
                                .expect("a block_on runs a shard");
                        assert_ne!(there, home);
                        super::wait_for_evictions(&[(token, src)], there);
                    });
                    let _ = done_tx.send(());
                })
            };
            assert!(
                done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
                "the other shard waited for this shard's registration"
            );
            other.join().unwrap();
            assert_eq!(
                src.registration(),
                (home, token),
                "the registration here moved"
            );
        });
        drop(handle);
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
        evict_all(gone, self.shared_addr());
        self.let_go_of_children();
        // The ring's operations still out are cancelled and their
        // completions waited for, so no memory they point at goes while the
        // kernel may write it.
        #[cfg(ring)]
        if let Driver::Ring(ring) = &self.driver {
            // Cancels a holder of a source on another thread left here.
            ring.settle(|| self.apply_io_ops());
            self.pool.unreserve(ring.shut_down());
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
        // The inbox's taker's role goes, looking at the inbox once more: a
        // pusher that saw the flag while the role was held left its
        // runnable to this drain.
        self.shared.drop_stopped(true);
    }
}
