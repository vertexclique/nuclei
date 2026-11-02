//! The hand-back: a call that needs its shard, from a thread whose poll lost that shard to the
//! monitor, gets it back for the length of the call from the thread that runs it now
//! (proofs/tla/handoff/HandBack.tla).
//!
//! The call ([`borrow`]) puts a lend task into the shard's queues, which go with the shard at
//! every take, and waits, its thread parked and running nothing, until the shard is lent or
//! the runtime shuts down. A runner polls the lend task ([`Lend`]) between its other polls, in
//! its loop or in a nested `block_on`'s, as any task: its code makes a full bracket, so the
//! runner's word says inside for the whole loan and the monitor takes nothing. A bracket that
//! finds the shard taken sends the task back into the shard's queues for its next runner. A
//! shard-local value lent on the runner's thread defers the loan a tick. Otherwise the runner
//! hands the shard's own queues back to their schedules, lends with a compare-and-swap from
//! asked to lent, and parks until the shard is back. Each step passes the shard with a release
//! and an acquire, as a take does, and the lender touches nothing of it meanwhile. A shutdown
//! wakes the waiting call, and a compare-and-swap from asked to gone settles it against a lend
//! that came first. Nothing here runs on a path that never detaches.

use core::future::{Future, poll_fn};
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use std::sync::Arc;
use std::time::Duration;

use crate::rt::context;
use crate::rt::sched::Sched;
use crate::rt::shard::Shard;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicU8, Ordering};

/// The call asked for the shard, and waits.
const ASKED: u8 = 0;
/// A runner lent the shard to the call.
const LENT: u8 = 1;
/// The call gave the shard back.
const BACK: u8 = 2;
/// The runtime shut down first: nothing is lent.
const GONE: u8 = 3;

/// One request for the shard: its state, and the wakers of the call and of the runner that
/// lends.
pub(crate) struct Loan {
    state: AtomicU8,
    borrower: AtomicWaker,
    lender: AtomicWaker,
}

impl Loan {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(ASKED),
            borrower: AtomicWaker::new(),
            lender: AtomicWaker::new(),
        }
    }

    /// The runner's lend, from asked to lent, and the call woken: whether it lent (the call
    /// was told of a shutdown first otherwise). Release: the call sees every write the runner
    /// made to the shard before.
    fn lend(&self) -> bool {
        let lent = self
            .state
            .compare_exchange(ASKED, LENT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if lent {
            self.borrower.wake();
        }
        lent
    }

    /// The runner waits, parked, for the shard back. Acquire: it sees every write the call
    /// made to the shard.
    fn wait_back(&self) {
        crate::sync::internal::block_on(poll_fn(|cx| {
            self.lender.register(cx.waker());
            if self.state.load(Ordering::Acquire) == BACK {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
    }

    /// The call gives the shard back, by return or by unwind, and wakes the runner. Release, as
    /// [`lend`](Self::lend).
    pub(crate) fn give_back(&self) {
        self.state.store(BACK, Ordering::Release);
        self.lender.wake();
    }
}

/// Asks the runner of `shard` now for it, from a thread whose poll lost it, and waits, the
/// thread parked and running nothing, until it is lent (the loan, for the guard that gives it
/// back) or the runtime shuts down (`None`). `shard` is kept alive by the caller's frame;
/// only its `Shared` and its monitor are touched here.
pub(crate) fn borrow(shard: &Shard) -> Option<Arc<Loan>> {
    let monitor = shard.monitor()?;
    // Enabled before the look at the close, so a close after it wakes the wait.
    let mut closing = pin!(monitor.closing());
    closing.as_mut().enable();
    if monitor.is_closed() {
        return None;
    }
    let loan = Arc::new(Loan::new());
    let lend = Lend {
        loan: Arc::clone(&loan),
        tick: monitor.tick(),
        deferred: None,
    };
    // The runtime's own task: not one of its user's, with no hooks. A stopped worker stays up
    // for it, as for this thread (`Shared::detached_began`).
    let (runnable, task) =
        crate::rt::task::spawn(lend, Sched::uncounted(Arc::clone(&shard.shared)));
    task.detach();
    shard.shared.push_remote(runnable);
    let lent = crate::sync::internal::block_on(poll_fn(|cx| {
        loan.borrower.register(cx.waker());
        if loan.state.load(Ordering::Acquire) == LENT {
            return Poll::Ready(true);
        }
        if closing.as_mut().poll(cx).is_ready() {
            // A lend that came first wins.
            let gone =
                loan.state
                    .compare_exchange(ASKED, GONE, Ordering::AcqRel, Ordering::Acquire);
            return Poll::Ready(gone.is_err());
        }
        Poll::Pending
    }));
    lent.then_some(loan)
}

/// The lend task: polled by whichever thread runs the shard, it lends the shard to its call
/// between that thread's polls.
struct Lend {
    loan: Arc<Loan>,
    /// How long a deferred loan waits before it tries again.
    tick: Duration,
    deferred: Option<Pin<Box<crate::time::Sleep>>>,
}

impl Future for Lend {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if let Some(wait) = this.deferred.as_mut() {
            if wait.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.deferred = None;
        }
        // A full bracket, held for the whole loan: the runner's word says inside meanwhile.
        let held = context::current_held();
        let Some(shard) = held.shard() else {
            // The shard was taken while this poll was outside it: this thread is detached, and
            // the wake, after the poll, sends the task to the shard's next runner.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        if context::lends_a_value() {
            // A value lent on this thread (up its stack, or to it) stays its own until it is
            // back: the loan waits a tick, the task still the shard's.
            let mut wait = Box::pin(crate::time::sleep(this.tick));
            if wait.as_mut().poll(cx).is_pending() {
                this.deferred = Some(wait);
            } else {
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        // No task bound to this thread may run on the call's: they go to their homes, the rest
        // to the shard's inbox.
        context::without_shard(|| shard.hand_over_queues());
        if this.loan.lend() {
            this.loan.wait_back();
        }
        Poll::Ready(())
    }
}
