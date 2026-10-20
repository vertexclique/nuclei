//! Task notification: wake one waiter, or every waiter, without a value.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::internal::{AtomicUsize, Ordering, UnsafeCell};
use super::list::List;
use super::queue::{Arrivals, FIRST_BIT, Policy, QUEUED, Step, WaitQueue};
use super::waiter::{Cancel, Wait, WakeList, release_queued};

/// A `notify_one` found nobody waiting and left this for the next waiter.
const PERMIT: usize = 1 << FIRST_BIT;

/// The payload a drainer leaves on a waiter it granted for `notify_one`,
/// so a waiter dropped before it saw the grant passes it on. Waiters
/// otherwise carry their generation, which never reaches this value.
const BY_ONE: usize = usize::MAX;

/// Wakes tasks waiting for an event that carries no value.
///
/// [`notify_one`](Self::notify_one) wakes the oldest waiter, or, when
/// nobody waits, stores one permit that the next
/// [`notified`](Self::notified) consumes at once, so a notification sent
/// just before the wait is not lost. [`notify_waiters`](Self::notify_waiters)
/// wakes every waiter that exists when it is called and stores nothing.
///
/// A [`Notified`] future receives `notify_waiters` calls made any time
/// after it was created, even before it is first polled.
///
/// ```
/// use nuclei::sync::Notify;
/// use std::future::Future;
/// use std::pin::pin;
/// use std::task::{Context, Poll, Waker};
///
/// let notify = Notify::new();
/// notify.notify_one();
/// let mut cx = Context::from_waker(Waker::noop());
/// let waiting = pin!(notify.notified());
/// assert!(waiting.poll(&mut cx).is_ready(), "the stored permit is consumed");
/// ```
pub struct Notify {
    queue: WaitQueue,
    waiting: UnsafeCell<List>,
    /// `notify_one` calls a drain pass has yet to serve.
    pending: AtomicUsize,
    /// How many `notify_waiters` calls there have been.
    generation: AtomicUsize,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for Notify {}
// SAFETY: as above.
unsafe impl Sync for Notify {}

impl Notify {
    loom_const_fn! {
        /// A `Notify` with nobody waiting and no stored permit.
        pub fn new() -> Self {
            Self {
                queue: WaitQueue::new(0),
                waiting: UnsafeCell::new(List::new()),
                pending: AtomicUsize::new(0),
                generation: AtomicUsize::new(0),
            }
        }
    }

    /// A future that completes on the next notification.
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            generation: self.generation(),
            wait: Wait::new(),
            done: false,
        }
    }

    /// Wakes the oldest waiter, or stores a permit when nobody waits. At
    /// most one permit is stored.
    pub fn notify_one(&self) {
        let before = self.transition(|state| {
            if state & QUEUED != 0 {
                Step::Keep
            } else {
                Step::Set(state | PERMIT)
            }
        });
        if before & QUEUED == 0 {
            return;
        }
        self.pending.fetch_add(1, Ordering::Release);
        self.transition(Step::Drain);
    }

    /// Wakes every task waiting now. Stores no permit.
    pub fn notify_waiters(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        // A read-modify-write even with nobody queued: a waiter that queues
        // concurrently either is seen here or, through the state word,
        // sees the new generation in its own pass.
        self.transition(|state| {
            if state & QUEUED != 0 {
                Step::Drain(state)
            } else {
                Step::Set(state)
            }
        });
    }

    /// Takes a stored permit if nobody is queued ahead.
    fn take_permit(&self) -> bool {
        let before = self.transition(|state| {
            if state & (QUEUED | PERMIT) == PERMIT {
                Step::Set(state & !PERMIT)
            } else {
                Step::Keep
            }
        });
        before & (QUEUED | PERMIT) == PERMIT
    }

    /// The current generation. The top bit is masked off so it never
    /// equals [`BY_ONE`]. Generations are compared, never ordered: a
    /// waiter could miss one only if 2^(usize::BITS - 1) `notify_waiters`
    /// calls passed while one drain pass stood still.
    fn generation(&self) -> usize {
        self.generation.load(Ordering::Acquire) & (usize::MAX >> 1)
    }
}

impl Policy for Notify {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, _wake_front: bool) -> usize {
        let pool = &self.queue;
        // Read after the arrivals were taken, so each arrival carries this
        // generation or an older one.
        let generation = self.generation();
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            // Every node a pass leaves queued carries the generation that
            // pass read, so the queued nodes are all of one generation:
            // a `notify_waiters` since then expired every one of them, and
            // granting from the front reaches them all.
            while let Some(node) = waiting.front(pool) {
                if node.as_ref().payload() == generation {
                    break;
                }
                waiting.pop_front();
                woken.grant(node);
            }
            // Generation order is not queue order: a waiter can pass its
            // check, stop before it queues, and queue after a newer one.
            // So each arrival is checked on its own, and only the current
            // generation is queued, which keeps the rule above.
            for node in arrivals {
                if node.as_ref().is_cancelled() {
                    release_queued(node, pool);
                } else if node.as_ref().payload() != generation {
                    woken.grant(node);
                } else {
                    waiting.push_back(node);
                }
            }
            waiting.tidy(&self.queue);
            let mut owed = self.pending.swap(0, Ordering::AcqRel);
            if waiting.front(pool).is_some()
                && self.queue.state.fetch_and(!PERMIT, Ordering::AcqRel) & PERMIT != 0
            {
                owed += 1;
            }
            while owed > 0 {
                let Some(node) = waiting.front(pool) else {
                    break;
                };
                waiting.pop_front();
                node.as_ref().set_payload(BY_ONE);
                if woken.grant(node) {
                    owed -= 1;
                }
            }
            if owed > 0 {
                self.queue.state.fetch_or(PERMIT, Ordering::AcqRel);
            }
            if waiting.is_empty() { QUEUED } else { 0 }
        })
    }
}

impl Drop for Notify {
    fn drop(&mut self) {
        let pool = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).clear(pool) });
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Notify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notify").finish_non_exhaustive()
    }
}

/// The future [`Notify::notified`] returns.
///
/// Dropping it after a `notify_one` reached it, but before it completed,
/// passes that notification to the next waiter.
#[must_use = "futures do nothing unless polled"]
pub struct Notified<'a> {
    notify: &'a Notify,
    generation: usize,
    wait: Wait,
    done: bool,
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(());
        }
        let notify = this.notify;
        let ready = if this.wait.is_queued() {
            this.wait.poll(&notify.queue, Some(cx.waker())).is_ready()
        } else if notify.generation() != this.generation || notify.take_permit() {
            true
        } else {
            notify.enqueue(&mut this.wait, this.generation, cx.waker());
            this.wait.poll(&notify.queue, None).is_ready()
        };
        this.done = ready;
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        match self.wait.cancel(&self.notify.queue) {
            Cancel::Idle => {}
            Cancel::Withdrawn(_) => self.notify.withdrawn(false),
            Cancel::Granted(BY_ONE) => self.notify.notify_one(),
            Cancel::Granted(_) => {}
        }
    }
}

impl fmt::Debug for Notified<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notified")
            .field("done", &self.done)
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use core::task::Waker;
    use std::sync::Arc;

    #[test]
    fn a_notification_before_the_wait_is_kept_once() {
        let notify = Notify::new();
        notify.notify_one();
        notify.notify_one();
        Polled::new(notify.notified()).ready();
        Polled::new(notify.notified()).pending();
    }

    #[test]
    fn notify_one_wakes_waiters_in_order() {
        let notify = Notify::new();
        let mut first = Polled::new(notify.notified());
        first.pending();
        let mut second = Polled::new(notify.notified());
        second.pending();
        notify.notify_one();
        assert_eq!((first.wakes(), second.wakes()), (1, 0));
        first.ready();
        notify.notify_one();
        second.ready();
        Polled::new(notify.notified()).pending();
    }

    #[test]
    fn notify_waiters_wakes_everyone_present_and_stores_nothing() {
        let notify = Notify::new();
        let mut waiting = Polled::new(notify.notified());
        waiting.pending();
        let mut created = Polled::new(notify.notified());
        notify.notify_waiters();
        waiting.ready();
        created.ready();
        Polled::new(notify.notified()).pending();
    }

    #[test]
    fn an_older_waiter_queued_behind_a_newer_one_still_gets_the_broadcast() {
        let notify = Notify::new();
        // Created before the broadcast, so the broadcast is owed to it.
        let mut older = notify.notified();
        notify.notify_waiters();
        let mut newer = Polled::new(notify.notified());
        newer.pending();
        // The older future's first poll passed its generation check before
        // the broadcast and queues only now, behind the newer waiter: the
        // rest of that poll, step by step.
        notify.enqueue(&mut older.wait, older.generation, Waker::noop());
        assert!(
            older.wait.poll(&notify.queue, None).is_ready(),
            "the broadcast after its creation reached it"
        );
        newer.pending();
        assert_eq!(newer.wakes(), 0, "an earlier broadcast is not its");
    }

    #[test]
    fn a_dropped_waiter_passes_its_notify_one_on() {
        let notify = Notify::new();
        let mut first = Polled::new(notify.notified());
        first.pending();
        let mut second = Polled::new(notify.notified());
        second.pending();
        notify.notify_one();
        drop(first);
        assert_eq!(second.wakes(), 1);
        second.ready();
    }

    #[test]
    fn one_notification_reaches_a_waiter_on_another_thread_whenever_it_lands() {
        let rounds = if cfg!(miri) { 5 } else { 200 };
        for _ in 0..rounds {
            let notify = Arc::new(Notify::new());
            let waiter = Arc::clone(&notify);
            let thread = std::thread::spawn(move || block_on(waiter.notified()));
            notify.notify_one();
            thread.join().expect("waiter");
        }
    }
}
