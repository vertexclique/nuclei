//! The children's exits a ring watches (see `process::exit`): a `WAITID`
//! operation on the child's pidfd where the kernel has it (Linux 6.7),
//! which reaps the child itself; else a poll of the pidfd, after which the
//! ring's thread reaps it. An operation's user data carries [`FILE_TAG`]
//! and [`EXIT_TAG`]; its memory (the info `WAITID` fills) stays in its slot
//! until its completion. A watch cancelled as the shard stops running lets
//! its child go only at that completion, so no other shard reaps the child
//! while a `WAITID` may still do it.

use std::sync::Arc;

use super::{EXIT_TAG, FILE_TAG, ShardRing};
use crate::process::exit::{Exit, status_of};
use crate::syscore::linux::iouring::ring::Sqe;

/// A child's exit, watched on a ring.
pub(crate) struct ExitOp {
    exit: Arc<Exit>,
    /// What `WAITID` fills; boxed, so it stays put while the call is out.
    info: Box<Info>,
    waitid: bool,
}

/// The info a `WAITID` fills.
struct Info(libc::siginfo_t);

// SAFETY: plain data the kernel fills; no pointer in it is ever followed.
unsafe impl Send for Info {}

#[cfg(test)]
thread_local! {
    /// This thread's ring watches children by a poll of the pidfd, as a ring
    /// without `WAITID` does.
    pub(crate) static NO_WAITID: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

fn no_waitid() -> bool {
    #[cfg(test)]
    return NO_WAITID.with(core::cell::Cell::get);
    #[cfg(not(test))]
    false
}

impl ShardRing {
    /// Watches the exit of `exit`'s child, which has a pidfd.
    pub(crate) fn watch_exit(&self, exit: Arc<Exit>) {
        let Some(pidfd) = exit.pidfd() else {
            return;
        };
        // SAFETY: plain data, all zeroes valid.
        let mut info = Box::new(Info(unsafe { core::mem::zeroed() }));
        let waitid = self.waitid && !no_waitid();
        let sqe = if waitid {
            Sqe::waitid(
                libc::P_PIDFD,
                pidfd as libc::id_t,
                &mut info.0,
                libc::WEXITED as u32,
            )
        } else {
            Sqe::poll_add(pidfd, libc::POLLIN as u32)
        };
        let op = ExitOp { exit, info, waitid };
        let inserted = self.exit_ops.borrow_mut().insert(op);
        match inserted {
            // The child may run for as long as it likes: never counted against the cap.
            Ok(user_data) => self.enter(user_data | FILE_TAG | EXIT_TAG, sqe, false),
            // Past 2^30 children watched at once on one ring.
            Err(op) => op.exit.finish(Err(libc::ENOBUFS)),
        }
    }

    /// The operation of a child's exit watch completed with `res`.
    pub(super) fn complete_exit(&self, user_data: u64, res: i32) {
        let Some(op) = self.take_exit(user_data) else {
            return;
        };
        self.end_exit(op, res);
    }

    fn take_exit(&self, user_data: u64) -> Option<ExitOp> {
        self.exit_ops
            .borrow_mut()
            .remove(user_data & !(FILE_TAG | EXIT_TAG))
    }

    /// A watch ended with `res`: the child's outcome, or (cancelled) the
    /// child let go.
    fn end_exit(&self, op: ExitOp, res: i32) {
        if res == -libc::ECANCELED {
            let orphan = Arc::strong_count(&op.exit) == 1;
            op.exit.unwatched(orphan);
            return;
        }
        if res < 0 {
            op.exit.finish(Err(-res));
        } else if op.waitid {
            // A wait for the exit only ever ends with one.
            match status_of(&op.info.0) {
                Some(status) => op.exit.finish(Ok(status)),
                None => op.exit.finish(Err(libc::ECHILD)),
            }
        } else {
            match op.exit.reap_pidfd() {
                Some(outcome) => op.exit.finish(outcome),
                // The poll fired with the child still running: once more.
                None => self.watch_exit(op.exit),
            }
        }
    }

    /// The shard stops running: each watch is cancelled, and its child let
    /// go at its completion.
    pub(crate) fn let_go_of_exits(&self) {
        let keys: Vec<u64> = self
            .exit_ops
            .borrow()
            .keys()
            .map(|key| key | FILE_TAG | EXIT_TAG)
            .collect();
        for user_data in keys {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel(user_data).user_data(super::IGNORED));
        }
    }
}
