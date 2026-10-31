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
//! and leaves the task's as it was. It lives in the thread's one context
//! (`rt::context`) next to the id of the task being polled, so a task's poll
//! saves and restores both through one access. Outside a budgeted poll, and
//! in an [`unconstrained`] one, the counter holds `UNBUDGETED`, which no
//! thread could spend in its life: an operation there spends without a branch
//! of its own, and never yields.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::rt::task::Id;

/// Operations one poll of a task may complete before it yields.
pub const BUDGET: u8 = 128;

/// The budget outside a budgeted poll: `2^64 - 1` operations, more than a
/// thread completes at one per nanosecond in 500 years, so it never runs out.
pub(crate) const UNBUDGETED: u64 = u64::MAX;

/// The budget left in this thread's poll.
#[inline(always)]
fn left() -> &'static Cell<u64> {
    &crate::rt::context::local().budget
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
#[inline(always)]
pub(crate) fn enter_task(id: Id) -> Entered {
    let here = crate::rt::context::local();
    Entered {
        left: here.budget.replace(u64::from(BUDGET)),
        task: here.task.replace(Some(id)),
    }
}

/// Puts back what [`enter_task`] replaced.
#[inline(always)]
pub(crate) fn leave_task(entered: Entered) {
    let here = crate::rt::context::local();
    here.budget.set(entered.left);
    here.task.set(entered.task);
}

/// The task being polled on this thread, if any.
#[inline]
pub(crate) fn current_task() -> Option<Id> {
    crate::rt::context::local().task.get()
}

/// Starts a poll's budget, and answers the budget before it.
#[inline]
pub(crate) fn enter() -> u64 {
    left().replace(u64::from(BUDGET))
}

/// Puts back the budget `enter` replaced.
#[inline]
pub(crate) fn leave(previous: u64) {
    left().set(previous);
}

/// Runs `f` outside the budget, as a call that does not wait (a `try_`
/// read or write) is: it neither spends nor yields.
#[inline]
pub(crate) fn outside<T>(f: impl FnOnce() -> T) -> T {
    let left = left();
    let saved = left.replace(UNBUDGETED);
    let result = f();
    left.set(saved);
    result
}

/// Spends one operation of the budget for an operation about to complete,
/// and answers whether it may. False when the budget is spent: nothing is
/// spent, and the operation completes nothing and yields.
#[inline(always)]
pub(crate) fn try_spend() -> bool {
    let left = left();
    // Stored before the branch, so the step is one decrement of the counter
    // in memory; a spent budget has its zero put back.
    let (rest, spent) = left.get().overflowing_sub(1);
    left.set(rest);
    if spent {
        was_spent();
    }
    !spent
}

#[cold]
#[inline(never)]
fn was_spent() {
    left().set(0);
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
    left().get() == 0 && !refused(waker)
}

/// Spends one operation of a budget [`exhausted`] just found not spent, for
/// the operation that then completed.
#[inline(always)]
pub(crate) fn spent_one() {
    let left = left();
    let rest = left.get();
    debug_assert_ne!(rest, 0, "spent without a look at the budget");
    left.set(rest.wrapping_sub(1));
}

/// Gives back the operation [`try_spend`] took, for one that did not
/// complete after all; read again, as a nested poll may have run its own
/// budget meanwhile.
#[inline]
fn refund() {
    let left = left();
    left.set(left.get().saturating_add(1));
}

/// Spends one operation of the budget for `poll`, or yields when it is spent:
/// `Pending`, with the task woken to run again after the others. A `poll`
/// that answers `Pending` gets its operation back.
#[inline]
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
    left().get() != 0
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
