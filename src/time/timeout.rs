//! [`Timeout`]: a future that gives up on another one at a deadline.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::time::Duration;

use super::Instant;

use super::error::Elapsed;
use super::sleep::{Sleep, far_future, sleep_until};

/// Runs `future` for at most `duration`: its output, or [`Elapsed`] if the
/// time ran out first. The future is polled before the clock is looked at,
/// so one that is ready at once always wins.
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    timeout_at(
        Instant::now()
            .checked_add(duration)
            .unwrap_or_else(far_future),
        future,
    )
}

/// Runs `future` until `deadline` at most; see [`timeout`].
pub fn timeout_at<F: Future>(deadline: impl Into<Instant>, future: F) -> Timeout<F> {
    Timeout {
        future,
        sleep: sleep_until(deadline),
    }
}

/// A future with a deadline. Made by [`timeout`] and [`timeout_at`].
#[must_use = "a timeout does nothing unless awaited"]
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
}

impl<F> Timeout<F> {
    /// The future inside.
    pub fn get_ref(&self) -> &F {
        &self.future
    }

    /// The future inside.
    pub fn get_mut(&mut self) -> &mut F {
        &mut self.future
    }

    /// The future inside, without its deadline.
    pub fn into_inner(self) -> F {
        self.future
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: the future is pinned structurally and never moved out while
        // pinned; the sleep is `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above, the future stays where it is.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        if let Poll::Ready(output) = future.poll(cx) {
            return Poll::Ready(Ok(output));
        }
        Pin::new(&mut this.sleep)
            .poll(cx)
            .map(|()| Err(Elapsed::new()))
    }
}

impl<F: fmt::Debug> fmt::Debug for Timeout<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timeout")
            .field("future", &self.future)
            .field("deadline", &self.sleep.deadline())
            .finish()
    }
}
