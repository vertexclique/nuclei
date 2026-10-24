//! The core of [`RwLock`](super::RwLock), which
//! [`ReentrantRwLock`](super::ReentrantRwLock) builds on.
//!
//! The state word holds the reader count, a writer bit, an upgradable bit
//! and an upgrading bit above the queue's bits.
//!
//! - **A bounded reader count.** A lock admits at most `max_readers`
//!   readers at once, which is the whole reader field unless the lock was
//!   built with a smaller bound ([`with_max_readers`](RawRwLock::with_max_readers)).
//!   A reader that finds the bound reached is not admitted: it waits like
//!   one held back by a writer, and a reader's release wakes the front.
//! - **Downgrade.** The holder of the write turns it into one read in a
//!   single step ([`downgrade`](RawRwLock::downgrade)): no writer or upgrade
//!   gets in between, and the readers queued behind a writer front are
//!   woken to share, as after a release.
//! - **Barging with bounded bypass, both ways.** A read enters whenever no
//!   writer holds the lock and no upgrade is under way; a write whenever
//!   the lock is free; either even past queued waiters, unless a handoff is
//!   owed (see `contend`). A release nudges the waiter at the front: a
//!   writer there is nudged on every release, so readers that keep
//!   overlapping still make it lose and count bypasses until it is owed a
//!   handoff, which holds every new reader back until the readers inside
//!   have left. Readers at the front are nudged together, with an
//!   upgradable read among them.
//! - **One upgradable read at a time.** It shares the lock with plain
//!   readers and excludes writers and other upgradable reads. Its upgrade
//!   sets the upgrading bit, which holds new readers back, and becomes the
//!   write once the readers inside have left. Since only one upgrader can
//!   exist, an upgrade never deadlocks on another. The upgradable bit and
//!   the upgrading bit are their holder's own from the moment it raises
//!   them until it takes the write or lets them go: a drain pass never
//!   changes them, it only tells the waiting upgrade that the readers have
//!   left.

#![allow(unsafe_code)]

use core::ptr::NonNull;
use core::task::{Poll, Waker};

use super::contend::{Contend, Lock};
use super::internal::{Ordering, UnsafeCell};
use super::list::List;
use super::queue::{
    Arrivals, FIRST_BIT, HANDOFF, Nudges, Policy, QUEUED, Step, WAKE_FRONT, WaitQueue, wants_drain,
};
use super::waiter::{Cancel, Park, Wait, Waiter, WakeList, release_queued};

const WRITER: usize = 1 << FIRST_BIT;
const UPGRADABLE: usize = 1 << (FIRST_BIT + 1);
/// The upgradable read is waiting to become the write.
const UPGRADING: usize = 1 << (FIRST_BIT + 2);
const READER_SHIFT: u32 = FIRST_BIT + 3;
const ONE_READER: usize = 1 << READER_SHIFT;
/// The most readers the reader field holds.
pub(super) const MAX_READERS: usize = usize::MAX >> READER_SHIFT;

/// What a waiter asks for.
pub(super) const READ: usize = 0;
pub(super) const WRITE: usize = 1;
pub(super) const UPGRADABLE_READ: usize = 2;
/// The upgradable read's wait to become the write.
const UPGRADE: usize = 3;

fn readers(state: usize) -> usize {
    state >> READER_SHIFT
}

/// `state` with `kind` entered. The caller checked that `kind` is admitted:
/// a read below `max_readers` leaves the reader field room.
fn enter(kind: usize, state: usize) -> usize {
    match kind {
        READ => state + ONE_READER,
        WRITE => state | WRITER,
        _ => state | UPGRADABLE,
    }
}

struct Waiting {
    list: List,
    /// The upgradable read's node while its upgrade waits.
    upgrade: Option<NonNull<Waiter>>,
}

/// The reader-writer core with no value attached.
pub(super) struct RawRwLock {
    queue: WaitQueue,
    waiting: UnsafeCell<Waiting>,
    /// The most readers at once, at least one and at most [`MAX_READERS`].
    max_readers: usize,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for RawRwLock {}
// SAFETY: as above.
unsafe impl Sync for RawRwLock {}

impl RawRwLock {
    loom_const_fn! {
        pub(super) fn new() -> Self {
            Self::with_max_readers(MAX_READERS)
        }
    }

    loom_const_fn! {
        /// A lock that admits at most `max_readers` readers at once. The
        /// caller keeps it between one and [`MAX_READERS`].
        pub(super) fn with_max_readers(max_readers: usize) -> Self {
            Self {
                queue: WaitQueue::new(0),
                waiting: UnsafeCell::new(Waiting { list: List::new(), upgrade: None }),
                max_readers,
            }
        }
    }

    loom_const_fn! {
        /// A lock whose contended acquires never spin (see
        /// `spin::Budget::never`).
        #[cfg(test)]
        pub(super) fn without_spin() -> Self {
            Self {
                queue: WaitQueue::without_spin(0),
                waiting: UnsafeCell::new(Waiting { list: List::new(), upgrade: None }),
                max_readers: MAX_READERS,
            }
        }
    }

    /// Whether `kind` could enter a lock in `state`, the owed bit aside.
    fn admits(&self, kind: usize, state: usize) -> bool {
        match kind {
            READ => state & (WRITER | UPGRADING) == 0 && readers(state) < self.max_readers,
            WRITE => state & (WRITER | UPGRADABLE) == 0 && readers(state) == 0,
            _ => state & (WRITER | UPGRADABLE) == 0,
        }
    }

    pub(super) fn try_kind(&self, kind: usize) -> bool {
        self.try_take(kind).is_ok()
    }

    /// Drops one read. One `fetch_sub` when no waiter needs telling.
    pub(super) fn read_unlock(&self) {
        let before = self.queue.state.fetch_sub(ONE_READER, Ordering::AcqRel);
        let upgrade_ready = readers(before) == 1 && before & UPGRADING != 0;
        if wants_drain(before) || upgrade_ready {
            self.released();
        }
    }

    /// Drops the write. One `fetch_and` when no waiter needs telling.
    pub(super) fn write_unlock(&self) {
        let before = self.queue.state.fetch_and(!WRITER, Ordering::AcqRel);
        if wants_drain(before) {
            self.released();
        }
    }

    /// Drops the upgradable read, and an upgrade it had begun.
    pub(super) fn upgradable_unlock(&self) {
        let before = self
            .queue
            .state
            .fetch_and(!(UPGRADABLE | UPGRADING), Ordering::AcqRel);
        if wants_drain(before) || before & UPGRADING != 0 {
            self.released();
        }
    }

    /// Turns the held write into a read, in one step: the writer bit goes
    /// down and the reader count goes up in the same compare-and-swap, so
    /// no writer or upgrade can enter between the two. The caller holds the
    /// write, so no reader is inside and the count has room for one.
    ///
    /// With somebody queued the step also asks for a pass, as a release
    /// does: the readers at the front can share now, and a front that cannot
    /// (a writer) is woken to count its loss and be owed the lock in the
    /// end.
    pub(super) fn downgrade(&self) {
        self.transition(|state| {
            debug_assert!(
                state & WRITER != 0 && readers(state) == 0,
                "a downgrade without the write"
            );
            let next = (state & !WRITER) + ONE_READER;
            if state & QUEUED != 0 {
                Step::Wake(next | WAKE_FRONT)
            } else {
                Step::Set(next)
            }
        });
    }

    /// Gives back what a granted `kind` holds.
    pub(super) fn unlock_kind(&self, kind: usize) {
        match kind {
            READ => self.read_unlock(),
            WRITE => self.write_unlock(),
            _ => self.upgradable_unlock(),
        }
    }

    /// Turns the held upgradable read into the write if no reader is
    /// inside.
    pub(super) fn try_upgrade(&self) -> bool {
        let mut state = self.queue.state.load(Ordering::Relaxed);
        loop {
            if readers(state) != 0 {
                return false;
            }
            match self.queue.state.compare_exchange_weak(
                state,
                (state & !(UPGRADABLE | UPGRADING)) | WRITER,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    /// Makes the held upgradable read the write, once a pass has told the
    /// upgrade that the readers have left. The caller still holds the
    /// upgradable read with its upgrade begun, which kept every reader and
    /// writer out since then: those two bits are up and the writer bit is
    /// down, so one flip of the three is the write.
    fn finish_upgrade(&self) {
        let before = self
            .queue
            .state
            .fetch_xor(UPGRADABLE | UPGRADING | WRITER, Ordering::AcqRel);
        debug_assert!(
            before & (UPGRADABLE | UPGRADING | WRITER) == UPGRADABLE | UPGRADING
                && readers(before) == 0,
            "an upgrade was granted on a lock its holder does not hold alone"
        );
    }

    /// Tells the waiting upgrade that the readers have left, by granting
    /// its node. False while it still waits.
    ///
    /// The pass changes nothing in the state word for it. The upgradable
    /// read and the upgrading bit stay the upgrader's own until its poll
    /// takes the write ([`RawRwLock::finish_upgrade`]) or its drop lets them
    /// go, so no pass can spend bits that a later holder has raised.
    fn grant_upgrade(&self, node: NonNull<Waiter>, woken: &mut WakeList<'_>) -> bool {
        let state = self.queue.state.load(Ordering::Acquire);
        if state & UPGRADING == 0 || readers(state) != 0 {
            return false;
        }
        // A node withdrawn meanwhile takes no grant, and leaves the slot too.
        woken.grant(node);
        true
    }
}

#[cfg(all(test, not(loom)))]
impl RawRwLock {
    pub(super) fn spin_rounds(&self) -> u32 {
        self.queue.spin.rounds()
    }
}

impl Lock for RawRwLock {
    fn admit(&self, state: usize, kind: usize) -> Option<usize> {
        self.admits(kind, state).then(|| enter(kind, state))
    }

    fn give_back(&self, kind: usize) {
        self.unlock_kind(kind);
    }
}

impl Policy for RawRwLock {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
        let queue = &self.queue;
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            // A dropped upgrade's node goes first: the upgradable read is
            // free once its holder dropped the upgrade, so the next upgrade's
            // node may be among these arrivals, and the slot is its place.
            if let Some(node) = waiting.upgrade
                && node.as_ref().is_cancelled()
            {
                release_queued(node, queue);
                waiting.upgrade = None;
            }
            for node in arrivals {
                if node.as_ref().is_cancelled() {
                    release_queued(node, queue);
                } else if node.as_ref().payload() == UPGRADE {
                    debug_assert!(waiting.upgrade.is_none(), "two upgrades at once");
                    waiting.upgrade = Some(node);
                } else {
                    waiting.list.push_back(node);
                }
            }
            waiting.list.tidy(queue);
            if let Some(node) = waiting.upgrade
                && self.grant_upgrade(node, woken)
            {
                waiting.upgrade = None;
            }
            let list = &mut waiting.list;
            if queue.state.load(Ordering::Acquire) & HANDOFF != 0 {
                while let Some(node) = list.front(queue) {
                    let kind = node.as_ref().payload();
                    if !self.take_for_waiter(kind) {
                        break;
                    }
                    list.pop_front();
                    if !woken.grant(node) {
                        self.undo(kind);
                    } else if kind == WRITE {
                        break;
                    }
                }
            }
            // While nobody is owed, the queue competes: wake the front if it
            // could enter, or after any release even if it cannot yet, so a
            // writer that overlapping readers keep out still loses, counts
            // the bypass, and is owed a handoff in the end. Readers and an
            // upgradable read behind a sharing front are woken with it
            // while they could share.
            if !queue.hands_off()
                && let Some(front) = list.front(queue)
            {
                let mut sharing = queue.state.load(Ordering::Acquire);
                let mut nudges = Nudges::new(woken, queue);
                for (at, node) in list.iter().enumerate() {
                    let kind = node.as_ref().payload();
                    let fits = self.admits(kind, sharing);
                    if at == 0 && (fits || wake_front) {
                        nudges.nudge(node);
                    }
                    if !fits || kind == WRITE || (at > 0 && front.as_ref().payload() == WRITE) {
                        break;
                    }
                    if at > 0 {
                        nudges.nudge(node);
                    }
                    sharing = enter(kind, sharing);
                }
                nudges.finish();
            }
            if list.front(queue).is_none() && waiting.upgrade.is_none() {
                QUEUED
            } else {
                0
            }
        })
    }
}

impl RawRwLock {
    /// Puts back a grant whose waiter withdrew in the meantime. The drain
    /// pass that called this goes on to reconsider the queue.
    fn undo(&self, kind: usize) {
        let gone = match kind {
            READ => ONE_READER,
            WRITE => WRITER,
            _ => UPGRADABLE,
        };
        if kind == READ {
            self.queue.state.fetch_sub(gone, Ordering::AcqRel);
        } else {
            self.queue.state.fetch_and(!gone, Ordering::AcqRel);
        }
    }
}

impl Drop for RawRwLock {
    fn drop(&mut self) {
        let queue = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            waiting.list.clear(queue);
            if let Some(node) = waiting.upgrade.take() {
                release_queued(node, queue);
            }
        });
    }
}

/// A future's wait for one side of a [`RawRwLock`].
pub(super) struct RwWait {
    contend: Contend,
}

impl RwWait {
    pub(super) const fn new() -> Self {
        Self {
            contend: Contend::new(),
        }
    }

    pub(super) fn is_queued(&self) -> bool {
        self.contend.is_queued()
    }

    pub(super) fn poll(&mut self, lock: &RawRwLock, kind: usize, waker: &Waker) -> Poll<()> {
        self.contend.poll(lock, kind, waker)
    }

    /// Withdraws a pending wait, passing on a grant made meanwhile.
    pub(super) fn cancel(&mut self, lock: &RawRwLock, kind: usize) {
        self.contend.cancel(lock, kind);
    }
}

/// The upgradable read's wait to become the write.
pub(super) struct UpgradeWait {
    wait: Wait,
    started: bool,
}

impl UpgradeWait {
    pub(super) const fn new() -> Self {
        Self {
            wait: Wait::new(),
            started: false,
        }
    }

    /// Ready once the caller, which holds the upgradable read, holds the
    /// write instead.
    pub(super) fn poll(&mut self, lock: &RawRwLock, waker: &Waker) -> Poll<()> {
        let park = if self.wait.is_queued() {
            self.wait.park(&lock.queue, Some(waker))
        } else {
            if !self.started {
                self.started = true;
                lock.queue.state.fetch_or(UPGRADING, Ordering::AcqRel);
            }
            if lock.try_upgrade() {
                return Poll::Ready(());
            }
            lock.enqueue(&mut self.wait, UPGRADE, waker);
            self.wait.park(&lock.queue, None)
        };
        match park {
            // A pass saw the last reader gone: the write is this caller's
            // to take, and nobody else's meanwhile.
            Park::Granted(_) => {
                lock.finish_upgrade();
                Poll::Ready(())
            }
            Park::Nudged | Park::Pending => Poll::Pending,
        }
    }

    /// Gives up an upgrade that has not completed, with the upgradable
    /// read the caller held. Whatever a pass told its node, the upgradable
    /// read is the caller's until this lets it go, exactly once.
    pub(super) fn cancel(&mut self, lock: &RawRwLock) {
        let outcome = self.wait.cancel(&lock.queue);
        lock.upgradable_unlock();
        if let Cancel::Withdrawn(_) = outcome {
            lock.withdrawn(false);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::cell::{Cell, RefCell};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as StdOrdering};
    use std::task::Wake;

    use super::*;

    /// A lock raced by other threads as each pass of a drain begins (each
    /// is told the pass's number, from 1).
    struct Raced<'a> {
        lock: &'a RawRwLock,
        passes: Cell<u32>,
        pass_begins: Box<dyn Fn(u32) + 'a>,
    }

    impl Policy for Raced<'_> {
        fn queue(&self) -> &WaitQueue {
            self.lock.queue()
        }

        fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
            let pass = self.passes.get() + 1;
            self.passes.set(pass);
            (self.pass_begins)(pass);
            self.lock.pass(arrivals, woken, wake_front)
        }
    }

    impl Lock for Raced<'_> {
        fn admit(&self, state: usize, kind: usize) -> Option<usize> {
            self.lock.admit(state, kind)
        }

        fn give_back(&self, kind: usize) {
            self.lock.give_back(kind);
        }
    }

    /// An upgrade waits for a reader, with its node already sorted by a
    /// pass. While another thread holds the drain role, that upgrade is
    /// dropped, a second task takes the upgradable read and starts its own
    /// upgrade, and the reader leaves. The second upgrade holds the
    /// upgradable read the whole time, so it must become the write, and no
    /// writer may get in before it.
    #[test]
    fn an_upgrade_dropped_while_another_thread_drains_does_not_cost_the_next_upgrade() {
        let lock = RawRwLock::new();
        let waker = Waker::noop();
        assert!(lock.try_kind(READ), "a reader is inside");
        assert!(lock.try_kind(UPGRADABLE_READ), "the first upgradable read");
        let first = RefCell::new(UpgradeWait::new());
        assert!(
            first.borrow_mut().poll(&lock, waker).is_pending(),
            "the first upgrade waits for the reader"
        );
        let second = RefCell::new(UpgradeWait::new());
        let raced = Raced {
            lock: &lock,
            passes: Cell::new(0),
            pass_begins: Box::new(|pass| {
                if pass == 1 {
                    first.borrow_mut().cancel(&lock);
                    assert!(lock.try_kind(UPGRADABLE_READ), "the second upgradable read");
                    assert!(
                        second.borrow_mut().poll(&lock, waker).is_pending(),
                        "the second upgrade waits for the reader"
                    );
                    lock.read_unlock();
                }
            }),
        };
        // A writer queues, and its poll takes the drain role.
        let mut writer = Contend::new();
        let wrote = writer.poll(&raced, WRITE, waker);
        assert!(
            wrote.is_pending(),
            "a writer got in while the upgradable read was held"
        );
        assert!(
            second.borrow_mut().poll(&lock, waker).is_ready(),
            "the upgrade never became the write"
        );
        writer.cancel(&raced, WRITE);
        lock.write_unlock();
    }

    /// A waker that, when the last copy of it is dropped, does what another
    /// thread could do at that instant: the reader leaves the lock, and a
    /// second task tries to take the upgradable read.
    struct RaceAtDrop {
        lock: Arc<RawRwLock>,
        second_took_it: Arc<AtomicBool>,
    }

    impl Wake for RaceAtDrop {
        fn wake(self: Arc<Self>) {
            // Nobody polls the future again. The race runs when the last
            // copy goes, which may be this one.
            drop(self);
        }
    }

    impl Drop for RaceAtDrop {
        fn drop(&mut self) {
            self.lock.read_unlock();
            let took = self.lock.try_kind(UPGRADABLE_READ);
            self.second_took_it.store(took, StdOrdering::Relaxed);
        }
    }

    /// An upgrade waits for a reader and is dropped. Its drop withdraws its
    /// node and then lets the upgradable read go; the node's waker is
    /// dropped in between, and there the reader leaves (which runs a pass)
    /// and a second task tries to take the upgradable read. Whoever holds
    /// the upgradable read afterwards, no writer may enter beside it.
    #[test]
    fn a_pass_between_a_dropped_upgrades_withdraw_and_its_release_frees_nothing_twice() {
        let lock = Arc::new(RawRwLock::new());
        assert!(lock.try_kind(READ), "a reader is inside");
        assert!(lock.try_kind(UPGRADABLE_READ), "the first upgradable read");
        let second_took_it = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(RaceAtDrop {
            lock: Arc::clone(&lock),
            second_took_it: Arc::clone(&second_took_it),
        }));
        let mut first = UpgradeWait::new();
        assert!(
            first.poll(&lock, &waker).is_pending(),
            "the first upgrade waits for the reader"
        );
        // The node holds the only other copy of the waker, so dropping the
        // upgrade drops it between the withdraw and the release.
        drop(waker);
        first.cancel(&lock);
        if !second_took_it.load(StdOrdering::Relaxed) {
            assert!(
                lock.try_kind(UPGRADABLE_READ),
                "the upgradable read is free once its holder let it go"
            );
        }
        assert!(
            !lock.try_kind(WRITE),
            "a writer got in while the upgradable read was held"
        );
        lock.upgradable_unlock();
        assert!(lock.try_kind(WRITE), "the lock is free at the end");
    }
}
