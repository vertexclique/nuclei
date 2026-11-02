//! What a shard does when the kernel dropped a completion: no operation waits for good.
//!
//! io_uring drops a completion when its completion queue is full and it has no memory for an
//! overflow entry. The operation it belonged to then never ends, and neither does the task that
//! waits on it. The poller counts the loss and asks the ring to recover (a no-op entry with
//! [`RECOVER`] as its user data, so the request reaches the ring as a completion). The ring then
//! notes every operation it has out, by user data, cancels them all (an operation still in the
//! kernel ends with `ECANCELED`, and its side or its call makes it again, as for any cancel), and
//! arms a short timeout ([`SWEEP`]). When it fires, each noted operation still in its table, found
//! by its exact user data so a slot reused since is never touched, is ended with an error:
//!
//! - a source's operation, and a child's exit watch, after the short wait: its side is told the
//!   operation was forsaken, and it counts out of the ring's live counts. Its memory, which the
//!   kernel may still write, moves into the recovery's own store, and its entry leaves the
//!   table, so a swept operation is in no table and counts as nothing in flight. The slab's
//!   generation keeps that safe: a slot reused later carries a newer one. Its own late
//!   completion, if one ever comes, finds nothing under its user data and reaches [`late`],
//!   which finds the memory by the exact user data and lets it go;
//! - a file's call, only once the linger time ran out counted from the start of the recovery: a
//!   call the kernel runs ignores a cancel and ends in its own time. Its slot gets the error and
//!   its memory is kept until its late completion.
//!
//! A zero-copy send that only holds its buffer until the peer acknowledged it ([`RingMem::ZcHeld`])
//! is not swept: no cancel ends that wait, and it ended for its side already. The same goes for
//! the zombies a shard's takeover leaves, which end by themselves. A recovery that starts while another is under way adds
//! its operations to it. Memory of a swept operation that never completes is kept for good when
//! the ring goes (see `keeper`). `proofs/tla/admission/Admission.tla` checks the design:
//! `G_dropped_recovers` is the design, `M_skip_sweep` the sweep left out and `M_no_cancel` the
//! cancel left out; each must fail on the invariant it names.

#[cfg(test)]
use core::cell::Cell;
use core::cell::RefCell;
use std::io;
use std::time::Instant;

use super::exits::ExitOp;
use super::keeper::{SHUTDOWN_WAIT, file_wait, keep_for_good};
use super::{EXIT_TAG, FILE_TAG, IGNORED, RECOVER, SWEEP, ShardRing, counts_as_write};
use crate::fs::ring::FileMem;
use crate::io::source::ops::{Done, Outcome};
use crate::io::source::ring::{RingMem, RingOp};
use crate::rt::shard::Shard;
use crate::syscore::linux::iouring::ring::Sqe;
use crate::syscore::linux::iouring::ring::sys::KernelTimespec;

/// The short wait between the cancel and the sweep of a source's operations, as the kernel reads
/// it after the entry is queued: static.
static SHORT_WAIT: KernelTimespec = KernelTimespec {
    tv_sec: SHUTDOWN_WAIT.as_secs() as i64,
    tv_nsec: SHUTDOWN_WAIT.subsec_nanos() as i64,
};

#[cfg(test)]
std::thread_local! {
    /// The short wait on this thread, in place of [`SHORT_WAIT`].
    pub(crate) static SWEEP_WAIT_FOR_TEST: Cell<Option<core::time::Duration>> = const { Cell::new(None) };
    /// Where the kernel reads the test's wait; thread-local, so its address stays valid.
    static TEST_WAIT: Cell<KernelTimespec> = const { Cell::new(KernelTimespec { tv_sec: 0, tv_nsec: 0 }) };
    /// Throws away the next completion of a source's operation that brought bytes, as if the
    /// kernel had dropped it, and counts the drop.
    pub(crate) static DROP_NEXT_FOR_TEST: Cell<bool> = const { Cell::new(false) };
}

/// The error a swept operation ends with.
pub(crate) fn dropped_error() -> io::Error {
    io::Error::other("the kernel dropped this operation's completion")
}

/// What an operation noted by a recovery is held in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Source,
    File,
    Exit,
}

/// An operation noted when a recovery began: its exact user data, and when it is swept if it has
/// not ended.
struct Note {
    user_data: u64,
    held: Held,
    due: Instant,
}

/// The operations a recovery under way still waits for, and how many it ended with the error.
struct Recovery {
    notes: Vec<Note>,
    ended: usize,
}

/// A ring's recovery: the one under way, the operations swept and the memory they keep until
/// their late completions.
#[derive(Default)]
pub(super) struct Recover {
    under_way: RefCell<Option<Recovery>>,
    /// Source operations swept, by user data, with the memory the kernel may still write; they
    /// are in no table.
    sources: RefCell<Vec<(u64, RingMem)>>,
    /// Files' calls swept, by user data, with the memory the kernel may still read or write.
    files: RefCell<Vec<(u64, FileMem)>>,
    /// Exit watches swept, by user data.
    exits: RefCell<Vec<(u64, ExitOp)>>,
}

/// Whether `mem` is an operation a recovery leaves alone: it ends by itself, or has ended for
/// its side already.
fn stays(mem: &RingMem) -> bool {
    matches!(
        mem,
        RingMem::ZcHeld(_) | RingMem::Zombie | RingMem::RecvZombie { .. }
    )
}

impl Recover {
    /// The late end of the source operation of `user_data`: its memory, if it was swept, goes.
    #[cold]
    #[inline(never)]
    pub(super) fn late(&self, user_data: u64) {
        let mut sources = self.sources.borrow_mut();
        if let Some(at) = sources.iter().position(|(u, _)| *u == user_data) {
            drop(sources.swap_remove(at));
        }
    }

    /// The ring goes: the swept sources' memory is kept for good (they ended for their sides
    /// already, and are in no table).
    pub(super) fn keep_sources(&self) {
        let sources = core::mem::take(&mut *self.sources.borrow_mut());
        if !sources.is_empty() {
            keep_for_good(Box::new(sources));
        }
    }

    /// The ring goes: what was swept and never completed is kept for good.
    pub(super) fn keep_leftovers(&self) {
        self.keep_sources();
        let files = core::mem::take(&mut *self.files.borrow_mut());
        if !files.is_empty() {
            keep_for_good(Box::new(files));
        }
        let exits = core::mem::take(&mut *self.exits.borrow_mut());
        if !exits.is_empty() {
            keep_for_good(Box::new(exits));
        }
    }
}

impl ShardRing {
    /// The wait before the sweep of a source's operations.
    fn short_wait() -> core::time::Duration {
        #[cfg(test)]
        if let Some(wait) = SWEEP_WAIT_FOR_TEST.with(Cell::get) {
            return wait;
        }
        SHUTDOWN_WAIT
    }

    /// Queues a timeout whose completion runs the sweep.
    fn arm_sweep(&self) {
        #[cfg(test)]
        if let Some(wait) = SWEEP_WAIT_FOR_TEST.with(Cell::get) {
            let at = TEST_WAIT.with(|t| {
                t.set(KernelTimespec {
                    tv_sec: wait.as_secs() as i64,
                    tv_nsec: i64::from(wait.subsec_nanos()),
                });
                t.as_ptr().cast_const()
            });
            self.poller
                .borrow_mut()
                .push(Sqe::timeout(at).user_data(SWEEP));
            return;
        }
        self.poller
            .borrow_mut()
            .push(Sqe::timeout(&SHORT_WAIT).user_data(SWEEP));
    }

    /// The poller asked for a recovery (see the module): every operation out now is noted and
    /// cancelled, and the sweep armed. A recovery under way takes the new ones in.
    #[cold]
    #[inline(never)]
    fn begin_recovery(&self) {
        let now = Instant::now();
        let (short, long) = (now + Self::short_wait(), now + file_wait());
        let mut found = Vec::new();
        {
            let mut ops = self.ops.borrow_mut();
            let keys: Vec<u64> = ops.keys().collect();
            for user_data in keys {
                if ops.get_mut(user_data).is_some_and(|op| !stays(&op.mem)) {
                    found.push(Note {
                        user_data,
                        held: Held::Source,
                        due: short,
                    });
                }
            }
        }
        // A call waiting for room is not in the kernel: no completion of it was dropped.
        let mut waiting: Vec<u64> = self.room.borrow().iter().map(|(u, _)| *u).collect();
        waiting.sort_unstable();
        for key in self.file_ops.borrow().keys() {
            let user_data = key | FILE_TAG;
            if waiting.binary_search(&user_data).is_err() {
                found.push(Note {
                    user_data,
                    held: Held::File,
                    due: long,
                });
            }
        }
        for key in self.exit_ops.borrow().keys() {
            found.push(Note {
                user_data: key | FILE_TAG | EXIT_TAG,
                held: Held::Exit,
                due: short,
            });
        }
        let mut under_way = self.recover.under_way.borrow_mut();
        let recovery = under_way.get_or_insert_with(|| Recovery {
            notes: Vec::new(),
            ended: 0,
        });
        recovery.notes.sort_unstable_by_key(|n| n.user_data);
        found.retain(|n| {
            recovery
                .notes
                .binary_search_by_key(&n.user_data, |m| m.user_data)
                .is_err()
        });
        if found.is_empty() {
            return;
        }
        let noted = found.len();
        recovery.notes.append(&mut found);
        drop(under_way);
        #[cfg(test)]
        let cancel = !super::keeper::SKIP_CANCEL_FOR_TEST.with(Cell::get);
        #[cfg(not(test))]
        let cancel = true;
        if cancel {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel_any(true).user_data(IGNORED));
        }
        self.arm_sweep();
        log::warn!(
            "the kernel dropped io_uring completions: {noted} operations were cancelled, and those that do not end will be ended with an error"
        );
    }

    /// The sweep's timeout completed with `res`: each noted operation that is due and still out
    /// is ended with the error. More to wait for arms the next timeout; none ends the recovery.
    #[cold]
    #[inline(never)]
    fn sweep(&self, shard: Option<&Shard>, res: i32) {
        // A cancelled timeout was cancelled by a later recovery, which armed its own.
        if res == -libc::ECANCELED {
            return;
        }
        let now = Instant::now();
        let due = {
            let mut under_way = self.recover.under_way.borrow_mut();
            let Some(recovery) = under_way.as_mut() else {
                return;
            };
            let (due, rest): (Vec<Note>, Vec<Note>) = core::mem::take(&mut recovery.notes)
                .into_iter()
                .partition(|n| n.due <= now);
            recovery.notes = rest;
            due
        };
        let mut ended = 0;
        for note in due {
            ended += usize::from(match note.held {
                Held::Source => self.sweep_source(shard, note.user_data),
                Held::File => self.sweep_file(note.user_data),
                Held::Exit => self.sweep_exit(note.user_data),
            });
        }
        let mut under_way = self.recover.under_way.borrow_mut();
        let Some(recovery) = under_way.as_mut() else {
            return;
        };
        recovery.ended += ended;
        if !recovery.notes.is_empty() {
            drop(under_way);
            self.arm_sweep();
            return;
        }
        let total = recovery.ended;
        *under_way = None;
        drop(under_way);
        if total > 0 {
            log::warn!(
                "the kernel dropped io_uring completions: {total} operations did not end and were ended with an error"
            );
        }
    }

    /// The source's operation of `user_data`, if still out, ends for its side with the error and
    /// counts out; its memory is kept until its own end. Whether it did.
    fn sweep_source(&self, shard: Option<&Shard>, user_data: u64) -> bool {
        let (op, write) = {
            let mut ops = self.ops.borrow_mut();
            if ops.get_mut(user_data).is_none_or(|op| stays(&op.mem)) {
                return false;
            }
            let Some(op) = ops.remove(user_data) else {
                return false;
            };
            let write = counts_as_write(&op);
            (op, write)
        };
        let RingOp {
            source,
            file,
            epoch,
            read,
            mem,
        } = op;
        self.recover.sources.borrow_mut().push((user_data, mem));
        if write {
            self.writes.set(self.writes.get().saturating_sub(1));
        }
        self.files.borrow_mut().done(file);
        source.op_done(
            Done {
                outcome: Outcome::Forsaken(dropped_error()),
                epoch,
                read,
            },
            shard,
        );
        true
    }

    /// The file's call of `user_data`, if still out, ends with the error in its slot, its memory
    /// kept until its late completion. Whether it did.
    fn sweep_file(&self, user_data: u64) -> bool {
        let Some(op) = self.file_ops.borrow_mut().remove(user_data & !FILE_TAG) else {
            return false;
        };
        self.in_kernel.set(self.in_kernel.get().saturating_sub(1));
        self.admit_waiting();
        let mem = op.forsake(dropped_error());
        self.recover.files.borrow_mut().push((user_data, mem));
        true
    }

    /// The child's exit watch of `user_data`, if still out, ends with an error for its waiter
    /// (`EIO`: the exit's outcome carries an errno), its memory kept until its late completion.
    /// Whether it did.
    fn sweep_exit(&self, user_data: u64) -> bool {
        let Some(op) = self.take_exit(user_data) else {
            return false;
        };
        op.finish_with(libc::EIO);
        self.recover.exits.borrow_mut().push((user_data, op));
        true
    }

    /// A completion for a file's call the ring does not have: if it was swept, its late end,
    /// and its memory goes.
    #[cold]
    #[inline(never)]
    pub(super) fn swept_file_ended(&self, user_data: u64) {
        let mut files = self.recover.files.borrow_mut();
        if let Some(at) = files.iter().position(|(u, _)| *u == user_data) {
            #[cfg(feature = "io-stats")]
            if self.timing.get().is_some() {
                self.timed(user_data, None, 0);
            }
            drop(files.swap_remove(at));
        }
    }

    /// A completion with the user data of the exit watches that finds no watch: a recovery's
    /// request or sweep, or the late end of a swept watch (its memory goes), or nothing the ring
    /// kept (a cancel's own).
    #[cold]
    #[inline(never)]
    pub(super) fn exit_unknown(&self, shard: Option<&Shard>, user_data: u64, res: i32) {
        match user_data {
            RECOVER => self.begin_recovery(),
            SWEEP => self.sweep(shard, res),
            _ => {
                let mut exits = self.recover.exits.borrow_mut();
                if let Some(at) = exits.iter().position(|(u, _)| *u == user_data) {
                    drop(exits.swap_remove(at));
                }
            }
        }
    }
}

#[cfg(test)]
impl ShardRing {
    /// Throws away `cqe` if the hook is set and it is a source's operation's, one-shot, with
    /// bytes: the poller then counts a drop and asks for a recovery. Whether it did.
    pub(super) fn drops_for_test(
        &self,
        cqe: &crate::syscore::linux::iouring::ring::IoUringCqe,
    ) -> bool {
        use crate::syscore::linux::iouring::ring::sys::IORING_CQE_F_MORE;
        if cqe.user_data & FILE_TAG != 0
            || cqe.res <= 0
            || cqe.flags & IORING_CQE_F_MORE != 0
            || !DROP_NEXT_FOR_TEST.with(Cell::get)
            || self.ops.borrow_mut().get_mut(cqe.user_data).is_none()
        {
            return false;
        }
        DROP_NEXT_FOR_TEST.with(|drop| drop.set(false));
        self.poller.borrow_mut().drop_for_test();
        true
    }

    /// Asks for a recovery as the poller does, with no completion lost.
    pub(crate) fn recover_for_test(&self) {
        self.poller.borrow_mut().ask_recovery();
    }

    /// Whether a recovery is under way.
    pub(crate) fn recovering(&self) -> bool {
        self.recover.under_way.borrow().is_some()
    }

    /// Source operations swept whose late completion has not come.
    pub(crate) fn swept_sources(&self) -> usize {
        self.recover.sources.borrow().len()
    }

    /// Files' calls swept whose late completion has not come.
    pub(crate) fn swept_files(&self) -> usize {
        self.recover.files.borrow().len()
    }

    /// Operations in the sources' table.
    pub(crate) fn table_len_for_test(&self) -> usize {
        self.ops.borrow().len()
    }
}
