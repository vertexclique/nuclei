//! A future's hold on its waiter node: queueing it, looking at it, spinning
//! on it and leaving the queue.

#![allow(unsafe_code)]

use core::ptr::NonNull;
use core::task::{Poll, Waker};

use super::node::{FUTURE_REF, Seen, Waiter, release};
#[cfg(all(loom, test))]
use super::spin_probe;
use crate::sync::queue::WaitQueue;
use crate::sync::spin::pause;

/// How a cancelled future left its queue.
pub(in crate::sync) enum Cancel {
    /// It never queued, or it already completed.
    Idle,
    /// It withdrew before anyone granted it; its owner tells the queue,
    /// passing on the nudge it carries unused, if it does.
    Withdrawn(bool),
    /// It had been granted, so its owner passes the grant on. Carries the
    /// payload the drainer left.
    Granted(usize),
}

/// What a future's look at its node found.
pub(in crate::sync) enum Park {
    /// Granted, with the payload the drainer left; the node is released.
    Granted(usize),
    /// A release asked it to try again; it is still queued.
    Nudged,
    /// Still waiting, its waker registered.
    Pending,
}

/// A future's hold on its waiter node.
pub(in crate::sync) struct Wait {
    node: Option<NonNull<Waiter>>,
}

impl Wait {
    pub(in crate::sync) const fn new() -> Self {
        Self { node: None }
    }

    #[inline]
    pub(in crate::sync) fn is_queued(&self) -> bool {
        self.node.is_some()
    }

    /// Arms a node from `queue`'s pool and pushes it on the arrival
    /// stack. The caller then publishes it with a draining transition and
    /// settles it with [`park`](Self::park) and no waker.
    pub(in crate::sync) fn enqueue(&mut self, queue: &WaitQueue, payload: usize, waker: &Waker) {
        debug_assert!(self.node.is_none(), "a future queues twice");
        let node = queue.pool().take();
        // SAFETY: fresh from the pool, so this future owns it alone.
        unsafe { node.as_ref() }.arm(payload, waker.clone());
        queue.push(node);
        self.node = Some(node);
    }

    /// Looks at the node; with `waker`, registers it first. Right after
    /// [`enqueue`](Self::enqueue) the waker is already in place, so pass
    /// `None`.
    pub(in crate::sync) fn park(&mut self, queue: &WaitQueue, waker: Option<&Waker>) -> Park {
        let Some(node) = self.node else {
            return Park::Granted(0);
        };
        // SAFETY: this future holds its reference until it clears `node`.
        let waiter = unsafe { node.as_ref() };
        match waiter.park(waker) {
            Seen::Granted => {
                let payload = waiter.payload();
                self.node = None;
                release(node, FUTURE_REF, queue);
                Park::Granted(payload)
            }
            Seen::Nudged => Park::Nudged,
            Seen::Pending => Park::Pending,
        }
    }

    /// After a lost retry: spins on the node for up to the queue's
    /// budget, then looks at it as [`park`](Self::park) with `waker` does.
    /// A grant or nudge that comes during the spin finds the node unarmed,
    /// so it only sets its mark, and the spin sees the mark; if the spin
    /// runs out, the look arms the waker and reads the marks in one
    /// compare-and-swap, so a mark set as the spin gave up is not lost.
    pub(in crate::sync) fn spin_park(&mut self, queue: &WaitQueue, waker: &Waker) -> Park {
        if let Some(node) = self.node {
            let rounds = queue.spin.rounds();
            // SAFETY: this future holds its reference until it clears `node`.
            let waiter = unsafe { node.as_ref() };
            if rounds > 0 && waiter.begin_spin() {
                #[cfg(all(loom, test))]
                spin_probe::count(&spin_probe::SPINS);
                if (0..rounds).any(|_| {
                    pause();
                    waiter.marked()
                }) {
                    // A grant or nudge landed on the node while it was
                    // unarmed, and the spin saw it.
                    #[cfg(all(loom, test))]
                    spin_probe::count(&spin_probe::MARKED_WHILE_SPINNING);
                    queue.spin.won(rounds);
                } else {
                    queue.spin.ran_out(rounds);
                    #[cfg(all(loom, test))]
                    {
                        if spin_probe::SKIP_LOOK.with(core::cell::Cell::get) {
                            return waiter.arm_without_look(waker);
                        }
                        let park = self.park(queue, Some(waker));
                        if !matches!(park, Park::Pending) {
                            // A mark landed after the spin's last read: only
                            // the look's swap saw it.
                            spin_probe::count(&spin_probe::MARKED_AFTER_LAST_READ);
                        }
                        return park;
                    }
                }
            }
        }
        self.park(queue, Some(waker))
    }

    /// The payload of the grant, if a drainer has granted the waiter; the
    /// node is released then, as [`park`](Self::park) does. Otherwise
    /// nothing: unlike `park`, it touches neither the waker nor the marks, so
    /// a caller with no waker to register can ask again and again while a
    /// grant is claiming the one already there.
    pub(in crate::sync) fn granted(&mut self, queue: &WaitQueue) -> Option<usize> {
        let node = self.node?;
        // SAFETY: this future holds its reference until it clears `node`.
        let waiter = unsafe { node.as_ref() };
        if !waiter.is_granted() {
            return None;
        }
        let payload = waiter.payload();
        self.node = None;
        release(node, FUTURE_REF, queue);
        Some(payload)
    }

    /// For a primitive that only grants: ready once granted.
    pub(in crate::sync) fn poll(
        &mut self,
        queue: &WaitQueue,
        waker: Option<&Waker>,
    ) -> Poll<usize> {
        match self.park(queue, waker) {
            Park::Granted(payload) => Poll::Ready(payload),
            Park::Nudged | Park::Pending => Poll::Pending,
        }
    }

    /// Marks the node owed a handoff. False when it already left the
    /// waiting phase.
    pub(in crate::sync) fn owe(&self) -> bool {
        let Some(node) = self.node else {
            return false;
        };
        // SAFETY: this future holds its reference.
        unsafe { node.as_ref() }.owe()
    }

    /// Takes back the node's owed mark. False when it already left the
    /// waiting phase, and the queue settles the mark.
    pub(in crate::sync) fn unowe(&self) -> bool {
        let Some(node) = self.node else {
            return false;
        };
        // SAFETY: this future holds its reference.
        unsafe { node.as_ref() }.unowe()
    }

    /// Leaves the queue: the future was dropped, or took what it waits
    /// for on its own. Reports a grant that got there first.
    pub(in crate::sync) fn cancel(&mut self, queue: &WaitQueue) -> Cancel {
        let Some(node) = self.node.take() else {
            return Cancel::Idle;
        };
        // SAFETY: this future held its reference until now.
        let waiter = unsafe { node.as_ref() };
        let outcome = match waiter.withdraw() {
            Some((waker, nudged)) => {
                drop(waker);
                Cancel::Withdrawn(nudged)
            }
            None => Cancel::Granted(waiter.payload()),
        };
        release(node, FUTURE_REF, queue);
        outcome
    }
}

// SAFETY: the node is shared only through the protocol in the module
// docs, which every access follows whichever thread makes it.
unsafe impl Send for Wait {}
// SAFETY: `&Wait` exposes nothing.
unsafe impl Sync for Wait {}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::wake::WakeList;
    use super::*;
    use crate::sync::test_support::Polled;

    /// A queue with one waiter armed, as after a poll that returned
    /// `Pending`, and the waker that counts its wakes.
    fn armed() -> (WaitQueue, Wait, Polled<core::future::Pending<()>>) {
        let queue = WaitQueue::new(0);
        let counter = Polled::new(core::future::pending::<()>());
        let mut wait = Wait::new();
        wait.enqueue(&queue, 1, &counter.waker());
        assert!(matches!(wait.park(&queue, None), Park::Pending));
        (queue, wait, counter)
    }

    fn node(wait: &Wait) -> NonNull<Waiter> {
        wait.node.expect("queued")
    }

    /// B, the spin on the waiter's own node. A nudge that finds the waiter
    /// spinning (disarmed by `begin_spin`) sets the mark and calls no
    /// waker; the look that ends the spin finds the mark.
    #[test]
    fn a_nudge_that_finds_its_waiter_spinning_marks_it_and_wakes_nobody() {
        let (queue, mut wait, counter) = armed();
        // SAFETY: the future holds its reference for the whole test.
        let waiter = unsafe { node(&wait).as_ref() };
        assert!(waiter.begin_spin(), "nothing marked yet: the waiter spins");
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)), "a nudge is on its way");
        drop(woken);
        assert_eq!(
            counter.wakes(),
            0,
            "a spinning waiter's waker is not called"
        );
        assert!(waiter.marked(), "the spin sees the mark");
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// The same nudge to an armed waiter claims its waker and calls it: what
    /// `begin_spin` saves a spinning waiter.
    #[test]
    fn a_nudge_that_finds_its_waiter_armed_calls_its_waker() {
        let (queue, mut wait, counter) = armed();
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)));
        drop(woken);
        assert_eq!(counter.wakes(), 1, "an armed waiter is woken");
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// A mark set just as the spin gives up is not lost: the look that arms
    /// the waker again reads the marks in the same compare-and-swap, so the
    /// waiter retries instead of sleeping beside it.
    #[test]
    fn a_nudge_as_the_spin_gives_up_is_seen_by_the_look() {
        let (queue, mut wait, counter) = armed();
        // SAFETY: as above.
        let waiter = unsafe { node(&wait).as_ref() };
        assert!(waiter.begin_spin());
        assert!(!waiter.marked(), "the spin runs out with nothing marked");
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)), "the nudge lands as it gives up");
        drop(woken);
        assert_eq!(counter.wakes(), 0);
        assert!(
            matches!(wait.park(&queue, Some(&counter.waker())), Park::Nudged),
            "the look found the nudge; the waiter does not sleep beside it"
        );
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// A node marked before the spin begins needs no spin: `begin_spin`
    /// says so and leaves the mark for the look.
    #[test]
    fn a_marked_node_does_not_spin() {
        let (queue, mut wait, counter) = armed();
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)));
        drop(woken);
        // SAFETY: as above.
        assert!(!unsafe { node(&wait).as_ref() }.begin_spin());
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }
}
