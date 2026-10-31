//! Cooperative scheduling: every poll of a task (and of `block_on`'s own
//! future) gets a budget of [`BUDGET`] operations. IO that completes spends
//! one; a task whose budget is spent is answered `Pending` and woken at once,
//! so it goes behind the other tasks of its shard instead of starving them
//! on IO that is always ready. An operation that does not complete is
//! refunded: only progress spends budget.
//!
//! The budget is a per-thread counter (in the thread's `rt::context`), saved
//! and restored around each poll, so a `block_on` nested in a task gives its
//! own polls a budget of their own and leaves the task's as it was.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Operations one poll of a task may complete before it yields.
pub const BUDGET: u8 = 128;

/// The budget left in this thread's poll; `None` outside a poll, or in an
/// [`unconstrained`] one.
#[inline(always)]
fn left() -> &'static Cell<Option<u8>> {
    &crate::rt::context::local().budget
}

/// Starts a poll's budget, and answers the budget before it.
#[inline]
pub(crate) fn enter() -> Option<u8> {
    left().replace(Some(BUDGET))
}

/// Puts back the budget `enter` replaced.
#[inline]
pub(crate) fn leave(previous: Option<u8>) {
    left().set(previous);
}

/// Runs `f` outside the budget, as a call that does not wait (a `try_`
/// read or write) is: it neither spends nor yields.
#[inline]
pub(crate) fn outside<T>(f: impl FnOnce() -> T) -> T {
    let left = left();
    let saved = left.replace(None);
    let result = f();
    left.set(saved);
    result
}

/// Spends one operation of the budget for `poll`, or yields when it is spent:
/// `Pending`, with the task woken to run again after the others. A `poll`
/// that answers `Pending` gets its operation back.
#[inline]
pub(crate) fn poll_budget<T>(
    cx: &mut Context<'_>,
    poll: impl FnOnce(&mut Context<'_>) -> Poll<T>,
) -> Poll<T> {
    let cell = left();
    match cell.get() {
        Some(0) => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        Some(n) => {
            cell.set(Some(n - 1));
            let result = poll(cx);
            if result.is_pending() {
                // No progress, nothing spent; read again, as `poll` may have
                // run a nested poll with a budget of its own.
                cell.set(cell.get().map(|m| m.saturating_add(1)));
            }
            result
        }
        None => poll(cx),
    }
}

/// Whether the current poll has budget left (always `true` outside a poll
/// and in an [`unconstrained`] one).
pub fn has_budget_remaining() -> bool {
    left().get() != Some(0)
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
        let previous = left().replace(None);
        let result = future.poll(cx);
        leave(previous);
        result
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
