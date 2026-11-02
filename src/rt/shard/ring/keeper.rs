//! A shard's ring as the shard goes, and after it went.
//!
//! The ring's operations are cancelled as the shard goes. On a ring another thread can reap
//! ([`ShardRing::reapable`]), what is still out then goes to a keeper with the ring
//! ([`hand_off`]): a job of the runtime's blocking pool, as a port's keeper is
//! (`rt::shard::port`), or where this thread reaches no pool, a thread of its own. The keeper
//! applies the ring's completions until none is out, and only then lets go of the ring, so each
//! operation's memory is kept exactly as long as the kernel holds it. A zero-copy send's buffer
//! is what needs it most: its notification frees it only once the peer acknowledged the bytes,
//! which may be long after its shard went, and every `Released` still resolves. The keeper also
//! applies what the shard's mailbox still gets (a source's holder on another thread leaves its
//! detach or cancel there), so a closed socket's file leaves the ring's table and its sends end;
//! what it cannot apply goes as `IoOp::gone` lets it (a registration asked of the shard is made
//! where its task polls next). It takes the shard's part in the park handshake, so a post finds it
//! parked and wakes it at once, not at the ring's next completion. Nothing takes the ring back:
//! its shard is gone. The keeper waits with no bound of its own: a peer that never reads keeps a
//! zero-copy send's notification, and so the keeper and the ring's memory, for good.
//!
//! On a single-issuer ring with deferred task work the kernel runs an operation's completion
//! only when the ring's own thread enters it, and refuses any other thread's enter, so no keeper
//! can take such a ring. It sends no zero-copy, and its shard waits a bounded time for what it
//! cancelled ([`ShardRing::shut_down`]): an operation still out after that is given up on. It
//! ends for whoever waits on it with an error and no longer counts as out, so neither its task
//! nor a retire of its source waits for good, while the memory the kernel may still write is kept
//! for good, and counted. `proofs/tla/lent/LentZc.tla` checks the keeper's completions,
//! `proofs/tla/keeper/Kept.tla` its mailbox and `proofs/tla/keeper/Forgot.tla` what is given up
//! on.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use kovan_queue::seg_queue::SegQueue;

use super::super::{IoOp, Shared};
use super::{FILE_TAG, FileOp, IGNORED, RecvGroup, RingMem, RingOp, ShardRing, Sqe};
use crate::io::source::ops::{Done, Outcome};
use crate::sync::oneshot;

/// How long a shard that goes waits for its ring's cancelled operations, when it keeps them.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

static LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Operations whose completion never came as their shard went: their memory is kept for good.
#[cfg(test)]
pub(crate) fn leaked() -> usize {
    LEAKED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Memory the kernel may still write after its ring gave up on it: kept for the process's life,
/// and reachable, so a leak check sees it kept rather than lost.
struct KeptForGood(#[expect(dead_code, reason = "only kept, never read")] Box<dyn core::any::Any>);

// SAFETY: nothing reads, moves out of or drops what it holds once it is kept: it only stays.
unsafe impl Send for KeptForGood {}

static KEPT: OnceLock<SegQueue<KeptForGood>> = OnceLock::new();

fn keep_for_good(what: Box<dyn core::any::Any>) {
    KEPT.get_or_init(SegQueue::new).push(KeptForGood(what));
}

/// What an operation given up on ends with.
fn given_up() -> io::Error {
    io::Error::other("the shard went before the kernel ended the operation")
}

/// An operation of a source given up on: its side gets [`given_up`] for its end, and the
/// operation counts out, while its memory is kept for good. A zero-copy send that waits only for
/// its notification ended for its side, and counted out, at its first completion: it only keeps
/// its buffer.
fn forsake(op: RingOp) {
    let RingOp {
        source,
        epoch,
        read,
        mem,
        ..
    } = op;
    let ended = matches!(mem, RingMem::ZcHeld(_));
    keep_for_good(Box::new(mem));
    if ended {
        return;
    }
    source.op_done(
        Done {
            outcome: Outcome::Forsaken(given_up()),
            epoch,
            read,
        },
        None,
    );
}

impl ShardRing {
    /// The shard is going, its sources detached: a file's call that waits for room ends here,
    /// never submitted, and the kernel is asked to cancel every operation it holds.
    pub(crate) fn cancel_all(&self) {
        let waiting: Vec<u64> = self.room.borrow_mut().drain(..).map(|(u, _)| u).collect();
        for user_data in waiting {
            // Never submitted: nothing points at its memory, and it goes to the pool.
            let op = self.file_ops.borrow_mut().remove(user_data & !FILE_TAG);
            if let Some(op) = op {
                drop(op.complete(-libc::ECANCELED));
            }
        }
        if self.in_flight() > 0 {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel_any(true).user_data(IGNORED));
        }
    }

    /// The shard of a ring no keeper can take is going, its sources detached: every operation
    /// still out is cancelled and its completion waited for, for at most [`SHUTDOWN_WAIT`]. One
    /// that has not completed by then is given up on ([`forget_stuck`](Self::forget_stuck)): it
    /// ends with an error, keeps its memory for good (the kernel may still write it), and is
    /// counted (`leaked`). Returns the memory of the receive group it let go, for the shard's
    /// ledger.
    pub(crate) fn shut_down(&self) -> usize {
        self.cancel_all();
        self.settle(|| {});
        self.forget_stuck()
    }

    /// Submits what is queued and waits until no operation is out, for at
    /// most [`SHUTDOWN_WAIT`]: what its sources cancelled as they went.
    /// `between` runs before each wait (the shard's mailbox, where a
    /// source's holder on another thread leaves its cancel).
    pub(crate) fn settle(&self, mut between: impl FnMut()) {
        let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
        loop {
            between();
            if self.in_flight() == 0 {
                return;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero()
                || self
                    .wait(None, Some(left.min(Duration::from_millis(10))))
                    .is_err()
            {
                return;
            }
        }
    }

    /// What is still out is given up on, and counted: a source's operation ends for its side
    /// with an error and counts out, a file's call fills its slot with one, and the memory the
    /// kernel may still write is kept for good (a zero-copy write's buffer with it, so its
    /// `Released` never resolves). A child's wait keeps its info and its pidfd for good. Returns
    /// the memory of the receive group let go: none while a receive that draws from it is out.
    pub(super) fn forget_stuck(&self) -> usize {
        let stuck: Vec<RingOp> = self.ops.borrow_mut().drain().collect();
        let stuck_files: Vec<FileOp> = self.file_ops.borrow_mut().drain().collect();
        let stuck_exits: Vec<_> = self.exit_ops.borrow_mut().drain().collect();
        LEAKED.fetch_add(
            stuck.len() + stuck_files.len() + stuck_exits.len(),
            core::sync::atomic::Ordering::Relaxed,
        );
        let receiving = stuck.iter().any(|op| {
            matches!(
                op.mem,
                RingMem::RecvMulti { .. } | RingMem::RecvFromMulti { .. }
            )
        });
        stuck.into_iter().for_each(forsake);
        for file in stuck_files {
            keep_for_good(Box::new(file.forsake(given_up())));
        }
        if !stuck_exits.is_empty() {
            keep_for_good(Box::new(stuck_exits));
        }
        let group = self.recv.borrow_mut().take();
        if receiving {
            // A receive still in the kernel may write the group's buffers: they stay registered
            // and mapped for good.
            keep_for_good(Box::new(group));
            return 0;
        }
        let freed = group.as_ref().map_or(0, RecvGroup::reservation);
        drop(group);
        freed
    }
}

/// A ring its shard left with operations out, with the shard's mailbox.
struct Kept {
    ring: ShardRing,
    shared: Arc<Shared>,
}

// SAFETY: the shard is gone, so from the hand-off on the keeper is the ring's one user, and the
// ring takes submissions and waits from any thread (`hand_off` takes no other kind). What its
// operations hold (buffers, holds on a source, a zero-copy write's buffer) may go to any thread.
unsafe impl Send for Kept {}

/// Hands `ring`, whose shard went with operations still out, to a keeper: a job of the blocking
/// pool of the runtime this thread is inside, else a thread of its own. A ring no keeper can
/// reap, or one neither can be had for, is shut down here instead ([`ShardRing::shut_down`]).
pub(crate) fn hand_off(ring: ShardRing, shared: Arc<Shared>) {
    if !ring.reapable() {
        ring.shut_down();
        shared.close_io();
        return;
    }
    let kept = Kept { ring, shared };
    if let Some(pool) = crate::rt::runtime::blocking_pool_inside() {
        // The pool runs every job it is given (on the submitting thread, if no thread of its can
        // start), so the ring is never dropped unkept.
        pool.spawn(move || kept.keep()).detach();
        return;
    }
    // The thread takes the ring only once it runs, so a thread that cannot start leaves it here.
    let (give, take) = oneshot::channel::<Kept>();
    let started = std::thread::Builder::new()
        .name("nuclei-ring-keeper".into())
        .spawn(move || {
            if let Ok(kept) = take.blocking_recv() {
                kept.keep();
            }
        });
    let unkept = match started {
        Ok(_) => give.send(kept).err(),
        Err(_) => Some(kept),
    };
    if let Some(kept) = unkept {
        kept.ring.shut_down();
        kept.shared.close_io();
    }
}

impl Kept {
    /// Applies the ring's completions, and what the shard's mailbox still gets, until no
    /// operation is out; then lets go of the ring. Before each wait it says it sleeps and looks at
    /// the mailbox once more (`ParkState::begin_park`): a post then either is seen here, or finds
    /// the flag up and notifies the ring's wake, which ends the wait.
    fn keep(self) {
        let Kept { ring, shared } = self;
        while ring.in_flight() > 0 {
            apply_mailbox(&ring, &shared);
            if !shared.park.begin_park(|| shared.io_ops.holds_any()) {
                continue;
            }
            let waited = ring.wait(None, None);
            shared.park.end_park();
            if waited.is_err() {
                // A ring that cannot be waited on: what is still out is given up on.
                ring.forget_stuck();
                break;
            }
        }
        // The ring goes with nothing queued: what the mailbox holds, and gets
        // from now on, is let go of.
        drop(ring);
        shared.close_io();
    }
}

/// What the shard's mailbox got for its ring after the shard went: a source's holder on another
/// thread leaves its detach or its cancel there. Nothing registers with a shard that went, nor
/// runs on it, so the rest goes as `IoOp::gone` lets it: a registration asked of the shard is
/// made where its task polls next, a child's watch where its user polls next.
fn apply_mailbox(ring: &ShardRing, shared: &Shared) {
    shared.io_ops.take_all(|op| match op {
        IoOp::RingDetach(slot, out, lease) => ring.detach(slot, out, lease),
        IoOp::RingCancel(user_data) => {
            ring.cancel(user_data);
        }
        other => other.gone(),
    });
}
