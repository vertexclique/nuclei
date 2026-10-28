//! A shard's completion port while the shard does not run.
//!
//! The operations still out on the port when the thread's outermost
//! `block_on` returns, or when the shard goes, may stay out for long: a TCP
//! send is never cancelled (see `io::source::port`), and ends only once its
//! peer reads. Their packets are a keeper's to deliver: a job of the
//! blocking pool that drains the port until none is out. The shard takes
//! the port back when it runs again; a keeper that has not started then
//! never touches the port, and one that keeps is stopped and waited for, so
//! one thread at most drains the port at any time. `proofs/tla/port/`
//! checks the hand-over.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Shard;
use crate::sync::internal::{AtomicU8, Ordering};

/// The keeper was asked for and has not started.
const ASKED: u8 = 0;
/// The keeper drains the port.
const KEEPING: u8 = 1;
/// The shard runs again and waits for the keeper to stop.
const STOPPING: u8 = 2;
/// The keeper is done with the port.
const DONE: u8 = 3;
/// The shard ran again before the keeper started: it never starts.
const CALLED_OFF: u8 = 4;

/// The hand-over of a shard's port to its keeper.
pub(crate) struct Keeper {
    state: AtomicU8,
    /// The shard's thread, which waits for a keeper it stops.
    thread: std::thread::Thread,
}

impl Shard {
    /// The shard stops running: what its port has ready now is delivered
    /// here, and the operations still out after that are handed to a
    /// keeper. Returns false when no blocking pool can be reached from here
    /// (the thread's runtime is gone).
    pub(super) fn hand_off_port(&self) -> bool {
        let port = &self.shared.port;
        while port.out() > 0
            && self
                .poll_io(usize::MAX, Some(Duration::ZERO))
                .is_ok_and(|events| events > 0)
        {}
        if port.out() == 0 {
            return true;
        }
        let keeper = Arc::new(Keeper {
            state: AtomicU8::new(ASKED),
            thread: std::thread::current(),
        });
        let job = {
            let (keeper, port) = (Arc::clone(&keeper), Arc::clone(port));
            move || {
                if keeper
                    .state
                    .compare_exchange(ASKED, KEEPING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return;
                }
                port.keep(
                    || keeper.state.load(Ordering::Acquire) == STOPPING,
                    // SAFETY: as on the shard: a packet of a source's key is
                    // one of its operations, its OVERLAPPED the first field
                    // of a slot the operation's strong reference keeps.
                    |entry| unsafe { crate::io::source::port::complete(entry) },
                );
                if keeper.state.swap(DONE, Ordering::AcqRel) == STOPPING {
                    keeper.thread.unpark();
                }
            }
        };
        if !crate::rt::runtime::spawn_blocking_inside(job) {
            return false;
        }
        *self.keeper.borrow_mut() = Some(keeper);
        true
    }

    /// The shard runs again: it takes its port back from the keeper, if
    /// one was asked for, before it drains the port itself.
    pub(crate) fn take_port_back(&self) {
        let Some(keeper) = self.keeper.borrow_mut().take() else {
            return;
        };
        let stopping = keeper
            .state
            .compare_exchange(ASKED, CALLED_OFF, Ordering::AcqRel, Ordering::Acquire)
            .or_else(|_| {
                keeper.state.compare_exchange(
                    KEEPING,
                    STOPPING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
            });
        if stopping != Ok(KEEPING) {
            // Called off before it started, or done already.
            return;
        }
        // Ends the keeper's wait; a wake left on the port after it stopped
        // only ends a wait of the shard's, which looks for work then.
        self.shared.notifier.notify();
        while keeper.state.load(Ordering::Acquire) != DONE {
            std::thread::park();
        }
    }

    /// Whether a keeper still has the port, or will.
    pub(super) fn port_kept(&self) -> bool {
        self.keeper
            .borrow()
            .as_ref()
            .is_some_and(|k| matches!(k.state.load(Ordering::Acquire), ASKED | KEEPING))
    }

    /// Waits, up to a bound, for the packets of the operations issued to the
    /// shard's port, for a shard that goes where no keeper can be had: each
    /// brings its side what it held. One that never comes keeps its source
    /// and its memory for good, so the kernel never writes memory that is
    /// gone.
    pub(super) fn settle_port(&self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.shared.port.out() > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero()
                || self
                    .poll_io(usize::MAX, Some(left.min(Duration::from_millis(10))))
                    .is_err()
            {
                return;
            }
        }
    }
}
