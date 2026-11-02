//! The hand-back's side of a thread's state: a call that needs the shard this thread's poll
//! lost gets it lent back for its length ([`lent_back`]), and the thread forgets the shard once
//! it is done with what it ran of it ([`ForgetLost`]). Nothing here runs on a path that never
//! detaches (proofs/tla/handoff/HandBack.tla).

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

use super::local;
#[cfg(not(target_arch = "wasm32"))]
use super::{Entered, Here, enter_with};
#[cfg(not(target_arch = "wasm32"))]
use crate::rt::handoff::slot::Slot;
use crate::rt::shard::Shard;

/// Why [`lent_back`] has no shard to lend the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unlent {
    /// This thread lost no shard: it runs on no worker.
    Off,
    /// The runtime shut down before the shard came back: no runner lends it any more.
    Gone,
}

/// Runs `f` with the shard this thread was detached from, lent back for the call by the thread
/// that runs it now: for a call that needs its shard ([`current_pinned`](super::current_pinned)
/// found none, or the thread's own) from a task's code whose poll lost the shard, or from a
/// bound task of the thread finished after it. The call asks, its thread parked and running nothing until a
/// runner lends the shard between its polls, then runs inside the shard, with no slot, and
/// gives it back by return or by unwind (proofs/tla/handoff/HandBack.tla, `W_ask` to
/// `W_back`). [`Unlent::Off`] where the thread lost none, [`Unlent::Gone`] once the runtime
/// shut down.
#[cold]
#[inline(never)]
pub(crate) fn lent_back<R>(f: impl FnOnce(Result<&Shard, Unlent>) -> R) -> R {
    let here = local();
    // SAFETY: `lost` points at a shard while the frame of `run_shard` that ran it holds it (see
    // `Here::lost`), and that frame is on this thread's stack below every call that reads it.
    let Some(lost) = (unsafe { here.lost.get().as_ref() }) else {
        return f(Err(Unlent::Off));
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(loan) = crate::rt::handoff::loan::borrow(lost) {
        let _lent = Lent::enter(here, lost, loan);
        return f(Ok(lost));
    }
    f(Err(Unlent::Gone))
}

/// A shard lent to this thread's call: inside it with no slot (the lender's bracket keeps the
/// monitor off), as a thread that runs it, until the guard drops. Then out again, the shard's
/// own queues handed back to their schedules (a task bound to this thread must not stay in
/// queues another thread runs), and the shard given back, by return or by unwind.
#[cfg(not(target_arch = "wasm32"))]
struct Lent<'a> {
    here: &'static Here,
    shard: &'a Shard,
    loan: Option<Arc<crate::rt::handoff::loan::Loan>>,
    entered: Option<Entered<'a>>,
    detached: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl<'a> Lent<'a> {
    fn enter(
        here: &'static Here,
        shard: &'a Shard,
        loan: Arc<crate::rt::handoff::loan::Loan>,
    ) -> Self {
        // A poll on the lent shard is not one that lost it (`polled`).
        let detached = here.detached.replace(0);
        here.lent.set(shard);
        Self {
            here,
            shard,
            loan: Some(loan),
            entered: Some(enter_with(here, shard, None)),
            detached,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Lent<'_> {
    fn drop(&mut self) {
        /// Gives the shard back on every way out of the hand-over below.
        struct Back(Option<Arc<crate::rt::handoff::loan::Loan>>);
        impl Drop for Back {
            fn drop(&mut self) {
                if let Some(loan) = self.0.take() {
                    loan.give_back();
                }
            }
        }
        let _back = Back(self.loan.take());
        drop(self.entered.take());
        self.here.lent.set(core::ptr::null());
        self.here.detached.set(self.detached);
        self.shard.hand_over_queues();
    }
}

/// Runs `f` as a thread that runs no shard, for the runner of a shard about to lend it: the
/// shard's own queues, handed back to their schedules in `f`, go to its inbox and to their
/// homes rather than back into those queues.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn without_shard<R>(f: impl FnOnce() -> R) -> R {
    /// Puts back the shard this thread runs, as `f` returns or unwinds.
    struct Restore(&'static Here, *const Shard);
    impl Drop for Restore {
        fn drop(&mut self) {
            self.0.current.set(self.1);
        }
    }
    let here = local();
    let _restore = Restore(here, here.current.replace(core::ptr::null()));
    f()
}

/// Whether a shard-local value is lent on this thread now: its slot is pinned (a `with` of the
/// runner's code is under way), or a shard is lent to its call ([`lent_back`]). A runner that
/// polls a lend task then defers the loan (proofs/tla/handoff/HandBack.tla, `WP_in`, `W_in`).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn lends_a_value() -> bool {
    let here = local();
    // SAFETY: as in `with_slot`.
    !here.lent.get().is_null() || unsafe { here.slot.get().as_ref() }.is_some_and(Slot::is_pinned)
}

/// Clears the shard this thread was detached from as it drops, the thread no longer one of
/// its live tasks: held by the frame of `run_shard` that holds that shard, and dropped before
/// the frame lets go of it.
pub(crate) struct ForgetLost;

impl Drop for ForgetLost {
    fn drop(&mut self) {
        // SAFETY: as in `lent_back`: the frame that holds this guard holds the shard.
        if let Some(lost) = unsafe { local().lost.replace(core::ptr::null()).as_ref() } {
            lost.shared.detached_ended();
        }
    }
}
