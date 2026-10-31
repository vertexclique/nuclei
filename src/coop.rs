//! Cooperative scheduling: every poll of a task (and of `block_on`'s own
//! future) gets a budget of [`BUDGET`] operations. IO that completes spends
//! one; a task whose budget is spent is answered `Pending` and woken at once,
//! so it goes behind the other tasks of its shard instead of starving them
//! on IO that is always ready. An operation that does not complete is
//! refunded: only progress spends budget.
//!
//! The budget is a per-thread counter, saved and restored around each poll,
//! so a `block_on` nested in a task gives its own polls a budget of their own
//! and leaves the task's as it was.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Operations one poll of a task may complete before it yields.
pub const BUDGET: u8 = 128;

std::thread_local! {
    /// The budget left in this thread's poll; `None` outside a poll, or in
    /// an [`unconstrained`] one.
    static LEFT: Cell<Option<u8>> = const { Cell::new(None) };
}

/// Starts a poll's budget, and answers the budget before it.
pub(crate) fn enter() -> Option<u8> {
    LEFT.try_with(|left| left.replace(Some(BUDGET)))
        .ok()
        .flatten()
}

/// Puts back the budget `enter` replaced.
pub(crate) fn leave(previous: Option<u8>) {
    let _ = LEFT.try_with(|left| left.set(previous));
}

/// Runs `f` outside the budget, as a call that does not wait (a `try_`
/// read or write) is: it neither spends nor yields.
pub(crate) fn outside<T>(f: impl FnOnce() -> T) -> T {
    let saved = LEFT.try_with(|left| left.replace(None)).ok().flatten();
    let result = f();
    let _ = LEFT.try_with(|left| left.set(saved));
    result
}

/// Spends one operation of the budget for `poll`, or yields when it is spent:
/// `Pending`, with the task woken to run again after the others. A `poll`
/// that answers `Pending` gets its operation back.
#[inline(always)]
pub(crate) fn poll_budget<T>(
    cx: &mut Context<'_>,
    poll: impl FnOnce(&mut Context<'_>) -> Poll<T>,
) -> Poll<T> {
    let left = LEFT.try_with(Cell::get).ok().flatten();
    match left {
        Some(0) => {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Some(n) => {
            let _ = LEFT.try_with(|cell| cell.set(Some(n - 1)));
        }
        None => {}
    }
    // One call of `poll`, so it is inlined here.
    let result = poll(cx);
    if left.is_some() && result.is_pending() {
        // No progress, nothing spent; read again, as `poll` may have run a
        // nested poll with a budget of its own.
        let _ = LEFT.try_with(|cell| cell.set(cell.get().map(|m| m.saturating_add(1))));
    }
    result
}

/// Whether the current poll has budget left (always `true` outside a poll
/// and in an [`unconstrained`] one).
pub fn has_budget_remaining() -> bool {
    LEFT.try_with(Cell::get).ok().flatten() != Some(0)
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
        let previous = LEFT.try_with(|left| left.replace(None)).ok().flatten();
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
