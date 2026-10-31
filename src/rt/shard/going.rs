//! A shard that stops running: for now (the outermost `block_on` of its
//! thread returned) or for good (dropped). What it watches goes, each to be
//! watched where its user polls next: the sources, each told; the
//! children, each once its registration ended. A port hands the operations
//! still out to a keeper; so does a ring another thread can reap, as its
//! shard is dropped (`ring::keeper`), and any other ring waits for them a
//! bounded time. A source whose writes left bytes behind that the kernel has
//! not taken goes to a linger that writes them out where it runs, for no
//! task of the handle's may poll that side again (`tla/going/Behind.tla`),
//! and which a runtime that drops waits for (`rt::linger`).

use core::future::poll_fn;
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
            let signal = self.signal();
            let output = self.run_then_leave(future.as_mut(), &signal);
            self.keep_signal(signal);
            output
        })
    }

    /// The outermost `block_on` of a thread outside the runtime returned,
    /// and this, its own shard, runs again only at its next one.
    pub(crate) fn leave(&self) {
        // A shard that never registered a source since it last left has
        // none to let go of.
        if !self.sources.borrow().is_untouched() {
            let gone: Vec<(u64, Arc<Source>)> = self.sources.borrow_mut().drain_tokens().collect();
            evict_all(&gone, self.shared_addr());
            // On a worker, or with none on this thread's shard, which runs it at
            // the thread's next `block_on`, as any task spawned here; a runtime
            // that drops waits for it (`rt::linger`).
            drain_unsent(&gone, |source| crate::rt::linger::hand(drain(source)));
        }
        self.let_go_of_children();
        // Sources that went before (a dropped handle's) may have left a
        // cancel queued, or their operations still out: the settle's first
        // step applies the one, and with none of the other it is over.
        #[cfg(ring)]
        if let Driver::Ring(ring) = &self.driver {
            if !self.shared.io_ops.is_empty() {
                self.apply_io_ops();
            }
            if ring.in_flight() != 0 {
                ring.settle(|| self.apply_io_ops());
            }
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
fn evict_all(gone: &[(u64, Arc<Source>)], home: usize) {
    for (_, source) in gone {
        source.evict();
    }
    wait_for_evictions(gone, home);
}

/// Hands each source of `gone`, evicted, whose write side holds bytes the
/// kernel has not taken to `spawn`, for a task that writes them out.
fn drain_unsent(gone: &[(u64, Arc<Source>)], spawn: impl Fn(Arc<Source>)) {
    gone.iter()
        .filter(|(_, source)| source.has_unsent())
        .for_each(|(_, source)| spawn(Arc::clone(source)));
}

/// Writes out what the write side of `source` holds, on the shard it runs on,
/// for at most the linger a dropped handle's bytes get.
async fn drain(source: Arc<Source>) {
    let _ = crate::time::timeout(
        crate::io::handle::LINGER,
        poll_fn(|cx| source.poll_drain(cx)),
    )
    .await;
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
        evict_all(&gone, self.shared_addr());
        // A worker that takes tasks drains them; with none the runtime is
        // going, and its drop does (`rt::linger`).
        drain_unsent(&gone, |source| crate::rt::linger::hand(drain(source)));
        self.let_go_of_children();
        // The ring's operations still out are cancelled, and their completions waited for or
        // left to a keeper, so no memory they point at goes while the kernel may write it.
        #[cfg(ring)]
        if let Driver::Ring(ring) = &self.driver {
            if ring.reapable() {
                // Cancels a holder of a source on another thread left here; the keeper applies
                // what comes later.
                self.apply_io_ops();
                ring.cancel_all();
            } else {
                ring.settle(|| self.apply_io_ops());
                self.pool.unreserve(ring.shut_down());
            }
        }
        // The port's operations to a keeper, unless one has them already.
        #[cfg(port)]
        if !self.port_kept() && !self.hand_off_port() {
            self.settle_port();
        }
        // No timer fires here again: the arm requests never applied orphan
        // their timers now, and those in the wheel as the loop below lets go
        // of them (`sleep::Armed`), so each sleep arms again where it is
        // polled, or its task, woken here, goes too.
        self.shared.drain_timer_ops();
        // What goes with a task may wake another of the shard's (a timer's
        // waker, a channel its future held): that one comes back through the
        // inbox, and is dropped here too, until none is left.
        loop {
            let lifo = self.lifo.take();
            let queue = core::mem::take(&mut *self.queue.borrow_mut());
            let timers = self.wheel.borrow_mut().take_wakers();
            let precise = self.precise.borrow_mut().take_wakers();
            let fired = core::mem::take(&mut *self.fired.borrow_mut());
            let mut quiet = lifo.is_none()
                && queue.is_empty()
                && timers.is_empty()
                && precise.is_empty()
                && fired.is_empty();
            drop((lifo, queue, timers, precise, fired));
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
        self.shared.release_inbox();
        // The mailbox closes, unless a keeper applies it from now on (and
        // closes it once it is done).
        #[cfg(ring)]
        let kept = self.keep_ring();
        #[cfg(not(ring))]
        let kept = false;
        if !kept {
            self.shared.close_io();
        }
    }
}

impl Shard {
    /// The shard is dropped: a ring another thread can reap goes to a keeper with what is still
    /// out once the ready completions are applied. Where no keeper can be had, the shard waits
    /// for them a bounded time, and what is still out keeps its memory for good.
    /// Whether the ring went to a keeper, which applies the shard's mailbox
    /// from then on.
    #[cfg(ring)]
    fn keep_ring(&mut self) -> bool {
        // A ring no keeper can reap was shut down as the shard went.
        let Driver::Ring(ring) = &self.driver else {
            return false;
        };
        if !ring.reapable() {
            return false;
        }
        // A look that fails leaves the ready completions to the keeper, whose wait meets the
        // same failure and keeps what is out for good.
        let looked = ring.wait(None, Some(std::time::Duration::ZERO)).is_ok();
        if looked && ring.in_flight() == 0 {
            return false;
        }
        let Driver::Ring(ring) = core::mem::replace(&mut self.driver, Driver::Gone) else {
            return false;
        };
        super::ring::keeper::hand_off(ring, Arc::clone(&self.shared));
        true
    }
}
