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
//! detach or cancel there), so a closed socket's file leaves the ring's table and its sends end.
//! Nothing takes the ring back: its shard is gone.
//!
//! On a single-issuer ring with deferred task work the kernel runs an operation's completion
//! only when the ring's own thread enters it, and refuses any other thread's enter, so no keeper
//! can take such a ring. It sends no zero-copy, and its shard waits a bounded time for what it
//! cancelled ([`ShardRing::shut_down`]): an operation still out after that keeps its memory for
//! good, and is counted. `proofs/tla/lent/LentZc.tla` checks the keeper's part.

use std::sync::Arc;
use std::time::Duration;

use super::super::{IoOp, Shared};
use super::{EXIT_TAG, ExitOp, FILE_TAG, FileOp, IGNORED, RecvGroup, RingOp, ShardRing, Sqe};
use crate::sync::oneshot;

/// How long a shard that goes waits for its ring's cancelled operations, when it keeps them.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

static LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Operations whose completion never came as their shard went: their memory is kept for good.
#[cfg(test)]
pub(crate) fn leaked() -> usize {
    LEAKED.load(core::sync::atomic::Ordering::Relaxed)
}

impl ShardRing {
    /// The shard is going, its sources detached: what waits for room ends here, never submitted,
    /// and the kernel is asked to cancel every operation it holds.
    pub(crate) fn cancel_all(&self) {
        let waiting: Vec<u64> = self.room.borrow_mut().drain(..).map(|(u, _)| u).collect();
        for user_data in waiting {
            // Never submitted: nothing points at its memory. A file's call
            // goes to the pool; a child goes to be watched elsewhere.
            if user_data & (FILE_TAG | EXIT_TAG) == FILE_TAG | EXIT_TAG {
                self.drop_waiting_exit(user_data);
            } else if user_data & FILE_TAG != 0 {
                let op = self.file_ops.borrow_mut().remove(user_data & !FILE_TAG);
                if let Some(op) = op {
                    drop(op.complete(-libc::ECANCELED));
                }
            } else {
                let op = self.ops.borrow_mut().remove(user_data);
                if let Some(op) = op {
                    self.end(None, op, -libc::ECANCELED);
                }
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
    /// that never completes keeps its memory for good (the kernel may still write it), and is
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

    /// What is still out keeps its memory for good, the kernel may still write it, and is
    /// counted. Returns the memory of the receive group let go: none while a receive is stuck.
    fn forget_stuck(&self) -> usize {
        let stuck: Vec<RingOp> = self.ops.borrow_mut().drain().collect();
        let stuck_files: Vec<FileOp> = self.file_ops.borrow_mut().drain().collect();
        let stuck_exits: Vec<ExitOp> = self.exit_ops.borrow_mut().drain().collect();
        LEAKED.fetch_add(
            stuck.len() + stuck_files.len() + stuck_exits.len(),
            core::sync::atomic::Ordering::Relaxed,
        );
        // A file's call still in the kernel keeps its buffer and its
        // descriptor for good, a child's wait its info and its pidfd.
        core::mem::forget(stuck_files);
        core::mem::forget(stuck_exits);
        let group = self.recv.borrow_mut().take();
        let freed = if stuck.is_empty() {
            let freed = group.as_ref().map_or(0, RecvGroup::reservation);
            drop(group);
            freed
        } else {
            // A receive still in the kernel may write the group's buffers:
            // they stay registered and mapped for good.
            core::mem::forget(group);
            0
        };
        core::mem::forget(stuck);
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
    }
}

impl Kept {
    /// Applies the ring's completions, and what the shard's mailbox still gets, until no
    /// operation is out; then lets go of the ring.
    fn keep(self) {
        let Kept { ring, shared } = self;
        while ring.in_flight() > 0 {
            apply_mailbox(&ring, &shared);
            if ring.wait(None, None).is_err() {
                // A ring that cannot be waited on: what is still out keeps its memory for good.
                ring.forget_stuck();
                return;
            }
        }
    }
}

/// What the shard's mailbox got for its ring after the shard went: a source's holder on another
/// thread leaves its detach or its cancel there. Nothing registers with a shard that went, nor
/// runs on it, so the rest only goes.
fn apply_mailbox(ring: &ShardRing, shared: &Shared) {
    while let Ok(op) = shared.io_ops.try_pop() {
        match op {
            IoOp::RingDetach(slot, out) => ring.detach(slot, out),
            IoOp::RingCancel(user_data) => ring.cancel(user_data),
            other => drop(other),
        }
    }
}
