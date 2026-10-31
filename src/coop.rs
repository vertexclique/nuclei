//! Cooperative scheduling: every poll of a task (and of `block_on`'s own
//! future) gets a budget of [`BUDGET`] operations. IO that completes spends
//! one, and so does every receive, acquire and lock of `nuclei::sync` that
//! completes; a task whose budget is spent is answered `Pending` and woken at
//! once, so it goes behind the other tasks of its shard instead of starving
//! them on IO or a channel that is always ready. An IO operation that does
//! not complete is refunded: only progress spends budget.
//!
//! The budget is a per-thread counter, saved and restored around each poll,
//! so a `block_on` nested in a task gives its own polls a budget of their own
//! and leaves the task's as it was. It shares one thread-local with the id of
//! the task being polled, so a task's poll saves and restores both with one
//! lookup. Outside a budgeted poll, and in an [`unconstrained`] one, the
//! counter holds `UNBUDGETED`, which no thread could spend in its life: an
//! operation there spends without a branch of its own, and never yields.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::rt::task::Id;

/// Operations one poll of a task may complete before it yields.
pub const BUDGET: u8 = 128;

/// The budget outside a budgeted poll: `2^64 - 1` operations, more than a
/// thread completes at one per nanosecond in 500 years, so it never runs out.
const UNBUDGETED: u64 = u64::MAX;

/// What this thread is polling.
struct Polling {
    /// The budget left in the poll; `UNBUDGETED` outside a poll, or in an
    /// [`unconstrained`] one.
    left: Cell<u64>,
    /// The task being polled; `None` outside a task (in `block_on`'s own
    /// future, on a blocking-pool thread).
    task: Cell<Option<Id>>,
}

std::thread_local! {
    static POLLING: Polling = const {
        Polling {
            left: Cell::new(UNBUDGETED),
            task: Cell::new(None),
        }
    };
}

/// What [`enter_task`] replaced, for [`leave_task`] to put back.
#[derive(Clone, Copy)]
pub(crate) struct Entered {
    left: u64,
    task: Option<Id>,
}

/// Starts the poll of task `id`: a fresh budget, and `id` as the current
/// task. Answers what it replaced (a `block_on` inside a task polls other
/// tasks in between).
#[inline]
pub(crate) fn enter_task(id: Id) -> Entered {
    POLLING
        .try_with(|p| Entered {
            left: p.left.replace(u64::from(BUDGET)),
            task: p.task.replace(Some(id)),
        })
        .unwrap_or(Entered {
            left: UNBUDGETED,
            task: None,
        })
}

/// Puts back what [`enter_task`] replaced.
#[inline]
pub(crate) fn leave_task(entered: Entered) {
    let _ = POLLING.try_with(|p| {
        p.left.set(entered.left);
        p.task.set(entered.task);
    });
}

/// The task being polled on this thread, if any.
#[inline]
pub(crate) fn current_task() -> Option<Id> {
    POLLING.try_with(|p| p.task.get()).ok().flatten()
}

/// Starts a poll's budget, and answers the budget before it.
#[inline]
pub(crate) fn enter() -> u64 {
    POLLING
        .try_with(|p| p.left.replace(u64::from(BUDGET)))
        .unwrap_or(UNBUDGETED)
}

/// Puts back the budget `enter` replaced.
#[inline]
pub(crate) fn leave(previous: u64) {
    let _ = POLLING.try_with(|p| p.left.set(previous));
}

/// Runs `f` outside the budget, as a call that does not wait (a `try_`
/// read or write) is: it neither spends nor yields.
pub(crate) fn outside<T>(f: impl FnOnce() -> T) -> T {
    let saved = POLLING
        .try_with(|p| p.left.replace(UNBUDGETED))
        .unwrap_or(UNBUDGETED);
    let result = f();
    leave(saved);
    result
}

/// Spends one operation of the budget for an operation about to complete,
/// and answers whether it may. False when the budget is spent: nothing is
/// spent, and the operation completes nothing and yields.
#[inline(always)]
pub(crate) fn try_spend() -> bool {
    POLLING
        .try_with(|p| {
            // Stored before the branch, so the step is one decrement of the
            // counter in memory; a spent budget has its zero put back.
            let (rest, spent) = p.left.get().overflowing_sub(1);
            p.left.set(rest);
            if spent {
                was_spent(p);
            }
            !spent
        })
        .unwrap_or(true)
}

#[cold]
#[inline(never)]
fn was_spent(p: &Polling) {
    p.left.set(0);
}

/// [`try_spend`] for an operation polled with `waker`: when the budget is
/// spent the task is woken, to run again after the shard's other tasks, and
/// the caller answers `Pending`.
#[inline(always)]
pub(crate) fn spend(waker: &Waker) -> bool {
    try_spend() || refused(waker)
}

#[cold]
#[inline(never)]
fn refused(waker: &Waker) -> bool {
    waker.wake_by_ref();
    false
}

/// Whether the budget is spent, for an operation that cannot undo what it
/// completes: it looks before it starts, and spends with [`try_spend`] once it
/// completed. When the budget is spent the task is woken and the caller
/// answers `Pending` without starting.
#[inline]
pub(crate) fn exhausted(waker: &Waker) -> bool {
    POLLING.try_with(|p| p.left.get()).unwrap_or(UNBUDGETED) == 0 && !refused(waker)
}

/// Spends one operation of a budget [`exhausted`] just found not spent, for
/// the operation that then completed.
#[inline(always)]
pub(crate) fn spent_one() {
    let _ = POLLING.try_with(|p| {
        let left = p.left.get();
        debug_assert_ne!(left, 0, "spent without a look at the budget");
        p.left.set(left.wrapping_sub(1));
    });
}

/// Gives back the operation [`try_spend`] took, for one that did not
/// complete after all; read again, as a nested poll may have run its own
/// budget meanwhile.
#[inline]
fn refund() {
    let _ = POLLING.try_with(|p| p.left.set(p.left.get().saturating_add(1)));
}

/// Spends one operation of the budget for `poll`, or yields when it is spent:
/// `Pending`, with the task woken to run again after the others. A `poll`
/// that answers `Pending` gets its operation back.
pub(crate) fn poll_budget<T>(
    cx: &mut Context<'_>,
    poll: impl FnOnce(&mut Context<'_>) -> Poll<T>,
) -> Poll<T> {
    if !spend(cx.waker()) {
        return Poll::Pending;
    }
    let result = poll(cx);
    if result.is_pending() {
        refund();
    }
    result
}

/// Whether the current poll has budget left (always `true` outside a poll
/// and in an [`unconstrained`] one).
pub fn has_budget_remaining() -> bool {
    POLLING.try_with(|p| p.left.get()).unwrap_or(UNBUDGETED) != 0
}

/// Spends one operation of the budget, yielding first when it is spent: a
/// loop of work that awaits nothing else can call it to stay fair.
pub async fn consume_budget() {
    core::future::poll_fn(|cx| poll_budget(cx, |_| Poll::Ready(()))).await
}

/// Runs `future` with no budget: none of its operations spends any, and it
/// never yields for the budget's sake.
pub fn unconstrained<F: Future>(future: F) -> Unconstrained<F> {
    Unconstrained { future }
}

/// The future of [`unconstrained`].
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Unconstrained<F> {
    future: F,
}

impl<F: Future> Future for Unconstrained<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is structurally pinned and never moved.
        let future = unsafe { self.map_unchecked_mut(|this| &mut this.future) };
        outside(|| future.poll(cx))
    }
}

/// Runs `future` under the budget: each of its polls spends one operation,
/// and it yields instead of being polled when the budget is spent.
pub fn cooperative<F: Future>(future: F) -> Cooperative<F> {
    Cooperative { future }
}

/// The future of [`cooperative`].
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Cooperative<F> {
    future: F,
}

impl<F: Future> Future for Cooperative<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is structurally pinned and never moved.
        let future = unsafe { self.map_unchecked_mut(|this| &mut this.future) };
        poll_budget(cx, |cx| future.poll(cx))
    }
}
