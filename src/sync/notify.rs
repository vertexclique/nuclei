//! Task notification: wake one waiter, or every waiter, without a value.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Arc;

use super::internal::{AtomicU64, AtomicUsize, Ordering, UnsafeCell};
use super::list::List;
use super::queue::{Arrivals, FIRST_BIT, Policy, QUEUED, Step, WaitQueue};
use super::waiter::{Cancel, Wait, WakeList, release_queued};

/// A `notify_one` or `notify_last` found nobody waiting and left this for
/// the next waiter.
const PERMIT: usize = 1 << FIRST_BIT;

/// The payload a drainer leaves on a waiter it granted for `notify_one`,
/// so a waiter dropped before it saw the grant passes it on. Waiters
/// otherwise carry their generation, which never reaches this value.
const BY_FIRST: usize = usize::MAX;

/// The same for `notify_last`, which the dropped waiter passes on as a
/// `notify_last`.
const BY_LAST: usize = usize::MAX - 1;

/// A count that reaches this stays there, and a pass reads it as "every waiter".
const FULL: u16 = u16::MAX;

/// One kind of owed wake, counted in one word together with the generation the counts were made
/// at: the generation's low 32 bits above two 16-bit counts, the current ones (made at that
/// generation) and the older ones. The model is `Owe`, `CurOf` and `OldOf` of
/// `proofs/tla/notify/Notify.tla`; the proof is "The owed word" of `proofs/lean/Nuclei/Notify.lean`.
struct Owed(AtomicU64);

/// The fields of a word [`Owed::take`] cleared.
struct Taken {
    tag: u32,
    cur: u16,
    old: u16,
}

impl Taken {
    fn decode(word: u64) -> Self {
        Self {
            tag: (word >> 32) as u32,
            cur: (word >> 16) as u16,
            old: word as u16,
        }
    }

    fn encode(tag: u32, cur: u16, old: u16) -> u64 {
        u64::from(tag) << 32 | u64::from(cur) << 16 | u64::from(old)
    }
}

/// Whether generation tag `g` is ahead of `t`, as the tag wraps, by less than half its room.
fn newer(g: u32, t: u32) -> bool {
    let d = g.wrapping_sub(t);
    d != 0 && d < 1 << 31
}

impl Owed {
    loom_const_fn! {
        fn new() -> Self {
            Self(AtomicU64::new(0))
        }
    }

    /// Adds one count made at `generation`: current if the word is of that generation, turning
    /// the word's counts older if it is newer, older if the word is already of a later one.
    fn owe(&self, generation: usize) {
        let g = generation as u32;
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let at = Taken::decode(word);
            let (tag, cur, old) = if at.tag == g {
                (g, at.cur.saturating_add(1), at.old)
            } else if newer(g, at.tag) {
                (g, 1, at.old.saturating_add(at.cur))
            } else {
                (at.tag, at.cur, at.old.saturating_add(1))
            };
            let next = Taken::encode(tag, cur, old);
            match self
                .0
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(seen) => word = seen,
            }
        }
    }

    /// Takes both counts and leaves the generation.
    fn take(&self) -> Taken {
        Taken::decode(self.0.fetch_and(!0xFFFF_FFFF, Ordering::AcqRel))
    }

    /// Adds the counts of `t` back: as they were if the word still has its tag, all as older if a
    /// newer count came since. The word's tag only moves forward, so a word given back is never
    /// newer than the word it lands in.
    fn give_back(&self, t: &Taken) {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let at = Taken::decode(word);
            let next = if at.tag == t.tag {
                Taken::encode(
                    at.tag,
                    at.cur.saturating_add(t.cur),
                    at.old.saturating_add(t.old),
                )
            } else {
                Taken::encode(
                    at.tag,
                    at.cur,
                    at.old.saturating_add(t.cur).saturating_add(t.old),
                )
            };
            match self
                .0
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(seen) => word = seen,
            }
        }
    }
}

/// Wakes tasks waiting for an event that carries no value.
///
/// [`notify_one`](Self::notify_one) wakes the oldest waiter, or, when
/// nobody waits, stores one permit that the next
/// [`notified`](Self::notified) consumes at once, so a notification sent
/// just before the wait is not lost. [`notify_last`](Self::notify_last) is
/// the same for the newest waiter.
/// [`notify_waiters`](Self::notify_waiters) wakes every waiter that exists
/// when it is called and stores nothing.
///
/// A [`Notified`] future receives `notify_waiters` calls made any time
/// after it was created, even before it is first polled. It receives
/// `notify_one` and `notify_last` only once it is polled or
/// [enabled](Notified::enable).
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
    owed_first: Owed,
    /// `notify_last` calls a drain pass has yet to serve.
    owed_last: Owed,
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
                owed_first: Owed::new(),
                owed_last: Owed::new(),
                generation: AtomicUsize::new(0),
            }
        }
    }

    loom_const_fn! {
        /// A `Notify` with nobody waiting and no stored permit, usable in a
        /// `static`.
        pub fn const_new() -> Self {
            Self::new()
        }
    }

    /// A future that completes on the next notification.
    ///
    /// It is guaranteed to receive `notify_waiters` calls from the moment it
    /// is created, polled or not. It receives `notify_one` and `notify_last`
    /// from its first poll, or its first call to
    /// [`enable`](Notified::enable).
    pub fn notified(&self) -> Notified<'_> {
        Notified {
            notify: self,
            core: Core::new(self),
        }
    }

    /// [`notified`](Self::notified) through an `Arc`, so the future can
    /// outlive the borrow and move between tasks.
    pub fn notified_owned(self: Arc<Self>) -> OwnedNotified {
        OwnedNotified {
            core: Core::new(&self),
            notify: self,
        }
    }

    /// Wakes the oldest waiter, or stores a permit when nobody waits. At
    /// most one permit is stored.
    pub fn notify_one(&self) {
        self.notify_with(&self.owed_first);
    }

    /// Wakes the newest waiter, or stores a permit when nobody waits: as
    /// [`notify_one`](Self::notify_one), but for the waiter that queued last
    /// instead of the one that queued first.
    pub fn notify_last(&self) {
        self.notify_with(&self.owed_last);
    }

    /// One owed wake of `strategy`, or a stored permit if nobody is queued.
    fn notify_with(&self, owed: &Owed) {
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
        // Read right before the count is added, so the count is never of an older generation
        // than one a broadcast made before this call.
        owed.owe(self.generation());
        self.transition(Step::Drain);
    }

    /// Wakes every task waiting now: every [`Notified`] created before this
    /// call, whether or not it has been polled. Stores no permit.
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
    /// equals [`BY_FIRST`]. Generations are compared, never ordered: a
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
        // Read after the arrivals were taken and before the owed words, so each arrival carries
        // this generation or an older one, and a count of an older one was made before the
        // broadcast that moved it on, or by a call that overlapped it.
        let generation = self.generation();
        let first = self.owed_first.take();
        let last = self.owed_last.take();
        let tag = generation as u32;
        // A word ahead of the pass is given back whole for the pass of its own generation.
        let split = |owed: &Owed, t: &Taken| {
            if t.tag == tag {
                (usize::from(t.cur), usize::from(t.old))
            } else if newer(t.tag, tag) {
                owed.give_back(t);
                (0, 0)
            } else {
                (0, usize::from(t.old.saturating_add(t.cur)))
            }
        };
        let ((first_cur, mut first_old), (last_cur, mut last_old)) = (
            split(&self.owed_first, &first),
            split(&self.owed_last, &last),
        );
        // A count at FULL is every waiter: it is never used up, so it is left over below.
        let full = usize::from(FULL);
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            // An expired node takes an older `notify_one`, then an older `notify_last`, as its wake.
            let mut expire = |woken: &mut WakeList<'_>, node| {
                let (old, by) = if first_old > 0 {
                    (&mut first_old, BY_FIRST)
                } else if last_old > 0 {
                    (&mut last_old, BY_LAST)
                } else {
                    woken.grant(node);
                    return;
                };
                node.as_ref().set_payload(by);
                if woken.grant(node) && *old != full {
                    *old -= 1;
                }
            };
            // Every node a pass leaves queued carries the generation that
            // pass read, so the queued nodes are all of one generation:
            // a `notify_waiters` since then expired every one of them, and
            // granting from the front reaches them all.
            while let Some(node) = waiting.front(pool) {
                if node.as_ref().payload() == generation {
                    break;
                }
                waiting.pop_front();
                expire(woken, node);
            }
            // Generation order is not queue order: a waiter can pass its
            // check, stop before it queues, and queue after a newer one.
            // So each arrival is checked on its own, and only the current
            // generation is queued, which keeps the rule above.
            for node in arrivals {
                if node.as_ref().is_cancelled() {
                    release_queued(node, pool);
                } else if node.as_ref().payload() != generation {
                    expire(woken, node);
                } else {
                    waiting.push_back(node);
                }
            }
            waiting.tidy(&self.queue);
            let served = |count: usize| if count == full { usize::MAX } else { count };
            let mut first = served(first_cur);
            let mut last = served(last_cur);
            if waiting.front(pool).is_some()
                && self.queue.state.fetch_and(!PERMIT, Ordering::AcqRel) & PERMIT != 0
            {
                first = first.saturating_add(1);
            }
            while first > 0 {
                let Some(node) = waiting.front(pool) else {
                    break;
                };
                waiting.pop_front();
                node.as_ref().set_payload(BY_FIRST);
                if woken.grant(node) {
                    first -= 1;
                }
            }
            while last > 0 {
                let Some(node) = waiting.back(pool) else {
                    break;
                };
                waiting.pop_back();
                node.as_ref().set_payload(BY_LAST);
                if woken.grant(node) {
                    last -= 1;
                }
            }
            if first > 0 || last > 0 || first_old > 0 || last_old > 0 {
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

/// What [`Notified`] and [`OwnedNotified`] share: the generation of the
/// `notify_waiters` count when the future was made, its place in the queue
/// and whether it completed.
struct Core {
    generation: usize,
    wait: Wait,
    done: bool,
}

impl Core {
    fn new(notify: &Notify) -> Self {
        Self {
            generation: notify.generation(),
            wait: Wait::new(),
            done: false,
        }
    }

    /// Looks for a notification, queueing on `notify` if there is none yet.
    /// With `waker`, registers it; without one (an `enable`), the wait is
    /// queued with a waker that does nothing, which the first poll replaces.
    /// True once the notification is here.
    fn drive(&mut self, notify: &Notify, waker: Option<&Waker>) -> bool {
        if self.done {
            return true;
        }
        let ready = if self.wait.is_queued() {
            match waker {
                Some(waker) => self.wait.poll(&notify.queue, Some(waker)).is_ready(),
                // An `enable` has no waker to register: it only looks.
                None => self.wait.granted(&notify.queue).is_some(),
            }
        } else if notify.generation() != self.generation {
            true
        } else if notify.take_permit() {
            // A `notify_waiters` between the look at the generation and the
            // take already owed this future its wake: the permit is the next
            // waiter's.
            if notify.generation() != self.generation {
                notify.notify_one();
            }
            true
        } else {
            notify.enqueue(
                &mut self.wait,
                self.generation,
                waker.unwrap_or(Waker::noop()),
            );
            self.wait.poll(&notify.queue, None).is_ready()
        };
        self.done = ready;
        ready
    }

    /// Leaves the queue; a `notify_one` or `notify_last` that reached the
    /// wait unseen goes to the next waiter.
    fn cancel(&mut self, notify: &Notify) {
        match self.wait.cancel(&notify.queue) {
            Cancel::Idle => {}
            Cancel::Withdrawn(_, _shelved) => notify.withdrawn(false),
            Cancel::Granted(BY_FIRST) => notify.notify_one(),
            Cancel::Granted(BY_LAST) => notify.notify_last(),
            Cancel::Granted(_) => {}
        }
    }
}

/// The future [`Notify::notified`] returns.
///
/// It is fused: once it completes, polling it again is ready at once.
///
/// Dropping it after a `notify_one` or `notify_last` reached it, but before
/// it completed, passes that notification to the next waiter.
#[must_use = "futures do nothing unless polled"]
pub struct Notified<'a> {
    notify: &'a Notify,
    core: Core,
}

impl Notified<'_> {
    /// Adds this future to the waiters that receive `notify_one` and
    /// `notify_last`, as its first poll would, without registering a waker.
    /// Polling does the same; this is for a caller that must be queued
    /// before it checks a condition, so a notification sent in between is
    /// not stored as a permit for someone else.
    ///
    /// `notify_waiters` is received from creation on, enabled or not.
    ///
    /// Returns true if the future is ready, which any later poll then
    /// reports at once. That is so when:
    ///
    /// 1. `notify_waiters` was called since the future was created;
    /// 2. this is the first `enable` or poll and the `Notify` held a
    ///    permit, which the call consumes; or
    /// 3. the future was enabled or polled before and has since been
    ///    notified.
    ///
    /// ```
    /// use nuclei::sync::Notify;
    /// use std::pin::pin;
    ///
    /// let notify = Notify::new();
    /// let mut waiting = pin!(notify.notified());
    /// assert!(!waiting.as_mut().enable(), "nothing was sent yet");
    /// notify.notify_one();
    /// assert!(waiting.as_mut().enable(), "the one sent after enable is its");
    /// ```
    pub fn enable(self: Pin<&mut Self>) -> bool {
        let this = self.get_mut();
        this.core.drive(this.notify, None)
    }
}

impl Future for Notified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.core.drive(this.notify, Some(cx.waker())) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        self.core.cancel(self.notify);
    }
}

impl fmt::Debug for Notified<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notified")
            .field("done", &self.core.done)
            .finish()
    }
}

/// The future [`Notify::notified_owned`] returns: [`Notified`] that keeps
/// its `Notify` alive itself.
#[must_use = "futures do nothing unless polled"]
pub struct OwnedNotified {
    notify: Arc<Notify>,
    core: Core,
}

impl OwnedNotified {
    /// As [`Notified::enable`].
    pub fn enable(self: Pin<&mut Self>) -> bool {
        let this = self.get_mut();
        this.core.drive(&this.notify, None)
    }
}

impl Future for OwnedNotified {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.core.drive(&this.notify, Some(cx.waker())) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for OwnedNotified {
    fn drop(&mut self) {
        self.core.cancel(&self.notify);
    }
}

impl fmt::Debug for OwnedNotified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedNotified")
            .field("done", &self.core.done)
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use core::task::Waker;

    fn word(owed: &Owed) -> (u32, u16, u16) {
        let at = Taken::decode(owed.0.load(Ordering::Acquire));
        (at.tag, at.cur, at.old)
    }

    #[test]
    fn owed_adds_current_turns_old_and_takes_by_generation() {
        let owed = Owed::new();
        owed.owe(7);
        owed.owe(7);
        assert_eq!(word(&owed), (7, 2, 0));
        owed.owe(8);
        assert_eq!(word(&owed), (8, 1, 2), "a newer tag turns the counts old");
        owed.owe(7);
        assert_eq!(word(&owed), (8, 1, 3), "an older tag adds an old count");
        let taken = owed.take();
        assert_eq!((taken.tag, taken.cur, taken.old), (8, 1, 3));
        assert_eq!(
            word(&owed),
            (8, 0, 0),
            "take clears the counts, keeps the tag"
        );
    }

    #[test]
    fn give_back_restores_a_word_of_the_same_tag() {
        let owed = Owed::new();
        owed.owe(5);
        owed.owe(5);
        owed.owe(4);
        let taken = owed.take();
        assert_eq!(word(&owed), (5, 0, 0));
        owed.give_back(&taken);
        assert_eq!(word(&owed), (5, 2, 1), "the word is as it was");
        let taken = owed.take();
        owed.owe(5);
        owed.give_back(&taken);
        assert_eq!(word(&owed), (5, 3, 1), "counts add to a new one of the tag");
    }

    #[test]
    fn give_back_onto_a_word_that_moved_on_adds_older() {
        let owed = Owed::new();
        owed.owe(5);
        owed.owe(5);
        owed.owe(4);
        let taken = owed.take();
        owed.owe(6);
        owed.give_back(&taken);
        assert_eq!(word(&owed), (6, 1, 3), "all three come back as older");
    }

    #[test]
    fn give_back_counts_stay_at_full() {
        let owed = Owed::new();
        for _ in 0..usize::from(FULL) + 5 {
            owed.owe(3);
        }
        let taken = owed.take();
        owed.owe(3);
        owed.give_back(&taken);
        assert_eq!(word(&owed), (3, FULL, 0));
        let taken = owed.take();
        owed.owe(4);
        owed.give_back(&taken);
        assert_eq!(word(&owed), (4, 1, FULL));
    }

    #[test]
    fn owed_orders_tags_across_the_wrap() {
        let owed = Owed::new();
        owed.owe(1);
        owed.owe(u32::MAX as usize);
        assert_eq!(word(&owed), (1, 1, 1), "tag 1 is older than u32::MAX");
        let owed = Owed::new();
        owed.owe(u32::MAX as usize);
        owed.owe(1);
        assert_eq!(word(&owed), (1, 1, 1), "tag u32::MAX is older than 1");
    }

    #[test]
    fn owed_counts_stay_at_full() {
        let owed = Owed::new();
        for _ in 0..usize::from(FULL) + 5 {
            owed.owe(3);
        }
        assert_eq!(word(&owed), (3, FULL, 0));
        owed.owe(4);
        assert_eq!(word(&owed), (4, 1, FULL));
        for _ in 0..usize::from(FULL) + 5 {
            owed.owe(3);
        }
        assert_eq!(word(&owed), (4, 1, FULL));
    }

    #[test]
    fn a_notify_one_after_a_broadcast_leaves_the_permit_for_a_later_waiter() {
        let notify = Notify::new();
        let mut old = Polled::new(notify.notified());
        old.pending();
        notify.notify_waiters();
        notify.notify_one();
        old.ready();
        Polled::new(notify.notified()).ready();
    }

    #[test]
    fn a_notify_one_before_a_broadcast_leaves_no_permit() {
        let notify = Notify::new();
        let mut old = Polled::new(notify.notified());
        old.pending();
        notify.notify_one();
        notify.notify_waiters();
        old.ready();
        Polled::new(notify.notified()).pending();
    }

    #[test]
    fn a_notification_before_the_wait_is_kept_once() {
        let notify = Notify::new();
        notify.notify_one();
        notify.notify_one();
        notify.notify_last();
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
    fn notify_last_wakes_the_newest_waiter() {
        let notify = Notify::new();
        let mut waiters: Vec<_> = (0..3).map(|_| Polled::new(notify.notified())).collect();
        for waiter in &mut waiters {
            waiter.pending();
        }
        notify.notify_last();
        assert_eq!(
            waiters.iter().map(Polled::wakes).collect::<Vec<_>>(),
            [0, 0, 1]
        );
        notify.notify_last();
        notify.notify_one();
        assert_eq!(
            waiters.iter().map(Polled::wakes).collect::<Vec<_>>(),
            [1, 1, 1],
            "one from the back, one from the front"
        );
        for waiter in &mut waiters {
            waiter.ready();
        }
    }

    #[test]
    fn notify_last_skips_the_waiters_that_withdrew_at_the_back() {
        let notify = Notify::new();
        let mut oldest = Polled::new(notify.notified());
        oldest.pending();
        let mut middle = Polled::new(notify.notified());
        middle.pending();
        let mut newest = Polled::new(notify.notified());
        newest.pending();
        drop(newest);
        notify.notify_last();
        assert_eq!((oldest.wakes(), middle.wakes()), (0, 1));
        middle.ready();
        let mut fresh = Polled::new(notify.notified());
        fresh.pending();
        drop(fresh);
        notify.notify_last();
        assert_eq!(oldest.wakes(), 1, "only the oldest is left");
        oldest.ready();
    }

    /// Both ends of the list stay linked through the sweeps that follow
    /// many withdrawals: a model queue decides who each call must wake.
    #[test]
    fn both_ends_stay_linked_after_withdrawals_and_sweeps() {
        let notify = Notify::new();
        let mut waiters: Vec<(usize, Polled<Notified<'_>>)> = (0..9)
            .map(|id| (id, Polled::new(notify.notified())))
            .collect();
        for (_, waiter) in &mut waiters {
            waiter.pending();
        }
        // Withdraw the middle of the line, enough of it to sweep.
        for gone in [4, 5, 3, 6, 2] {
            let at = waiters
                .iter()
                .position(|(id, _)| *id == gone)
                .expect("queued");
            drop(waiters.remove(at));
        }
        let mut model: std::collections::VecDeque<usize> = [0, 1, 7, 8].into();
        let mut woken = Vec::new();
        for call in 0..4 {
            let expected = if call % 2 == 0 {
                notify.notify_last();
                model.pop_back()
            } else {
                notify.notify_one();
                model.pop_front()
            }
            .expect("a waiter is left");
            let (at, _) = waiters
                .iter()
                .enumerate()
                .find(|(_, (_, w))| w.wakes() == 1)
                .expect("somebody was woken");
            let (id, mut waiter) = waiters.remove(at);
            assert_eq!(id, expected, "call {call} woke the wrong waiter");
            waiter.ready();
            woken.push(id);
        }
        assert_eq!(woken, [8, 0, 7, 1]);
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
        notify.enqueue(&mut older.core.wait, older.core.generation, Waker::noop());
        assert!(
            older.core.wait.poll(&notify.queue, None).is_ready(),
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
    fn a_dropped_waiter_passes_its_notify_last_on_to_the_newest() {
        let notify = Notify::new();
        let mut oldest = Polled::new(notify.notified());
        oldest.pending();
        let mut middle = Polled::new(notify.notified());
        middle.pending();
        let mut newest = Polled::new(notify.notified());
        newest.pending();
        notify.notify_last();
        drop(newest);
        assert_eq!(
            (oldest.wakes(), middle.wakes()),
            (0, 1),
            "the pass-on wakes the newest left"
        );
        middle.ready();
        oldest.pending();
    }

    #[test]
    fn enable_queues_the_future_for_notify_one_without_a_poll() {
        let notify = Notify::new();
        let mut waiting = core::pin::pin!(notify.notified());
        assert!(!waiting.as_mut().enable());
        notify.notify_one();
        assert!(
            waiting.as_mut().enable(),
            "the notification found it queued"
        );
        assert!(waiting.as_mut().enable(), "and stays ready");
        // A permit sent after is left for the next waiter.
        notify.notify_one();
        Polled::new(notify.notified()).ready();
    }

    #[test]
    fn enable_consumes_a_stored_permit_and_sees_a_broadcast() {
        let notify = Notify::new();
        notify.notify_one();
        let mut got = core::pin::pin!(notify.notified());
        assert!(got.as_mut().enable(), "the stored permit");
        Polled::new(notify.notified()).pending();

        let mut broadcast = core::pin::pin!(notify.notified());
        notify.notify_waiters();
        assert!(
            broadcast.as_mut().enable(),
            "a broadcast after its creation"
        );
    }

    #[test]
    fn a_poll_after_enable_registers_the_real_waker() {
        let notify = Notify::new();
        let mut waiting = core::pin::pin!(notify.notified());
        assert!(!waiting.as_mut().enable());
        let probe = Polled::new(core::future::pending::<()>());
        let waker = probe.waker();
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        notify.notify_one();
        assert_eq!(
            probe.wakes(),
            1,
            "the waker of the poll replaced the empty one"
        );
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
    }

    #[test]
    fn an_enabled_future_is_ahead_of_one_polled_later() {
        let notify = Notify::new();
        let mut enabled = core::pin::pin!(notify.notified());
        assert!(!enabled.as_mut().enable());
        let mut polled = Polled::new(notify.notified());
        polled.pending();
        notify.notify_one();
        assert!(
            enabled.as_mut().enable(),
            "the enabled one is first in line"
        );
        assert_eq!(polled.wakes(), 0);
        notify.notify_one();
        assert_eq!(polled.wakes(), 1);
        polled.ready();
    }

    #[test]
    fn an_enabled_future_dropped_after_its_notification_passes_it_on() {
        let notify = Notify::new();
        let mut enabled = Box::pin(notify.notified());
        assert!(!enabled.as_mut().enable());
        let mut next = Polled::new(notify.notified());
        next.pending();
        notify.notify_one();
        drop(enabled);
        assert_eq!(next.wakes(), 1, "the unseen notification moved on");
        next.ready();
    }

    #[test]
    fn an_owned_notified_outlives_the_borrow() {
        let notify = Arc::new(Notify::new());
        let mut waiting = Polled::new(Arc::clone(&notify).notified_owned());
        waiting.pending();
        notify.notify_one();
        waiting.ready();
        let owned = Arc::clone(&notify).notified_owned();
        drop(notify);
        let mut owned = core::pin::pin!(owned);
        assert!(!owned.as_mut().enable());
    }

    #[test]
    fn a_const_notify_works_in_a_static() {
        static NOTIFY: Notify = Notify::const_new();
        NOTIFY.notify_one();
        Polled::new(NOTIFY.notified()).ready();
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
