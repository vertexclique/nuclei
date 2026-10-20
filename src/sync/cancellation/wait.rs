//! The futures of a token: the borrowed and the owned wait for cancellation,
//! and the race of a future against one.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll};

use pin_project_lite::pin_project;

use super::CancellationToken;
use crate::sync::event::{Event, EventWait};

pin_project! {
    /// `fut`, or `None` if the token is cancelled first; `fut` wins a tie.
    pub(super) struct RunUntilCancelled<'a, F: Future> {
        #[pin]
        pub(super) cancellation: WaitForCancellationFuture<'a>,
        #[pin]
        pub(super) future: F,
    }
}

impl<F: Future> Future for RunUntilCancelled<'_, F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(output) = this.future.poll(cx) {
            Poll::Ready(Some(output))
        } else if this.cancellation.poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

/// A future that resolves once the corresponding [`CancellationToken`] is
/// cancelled.
///
/// Created by [`CancellationToken::cancelled`].
#[must_use = "futures do nothing unless polled"]
pub struct WaitForCancellationFuture<'a> {
    pub(super) wait: EventWait<'a>,
}

impl fmt::Debug for WaitForCancellationFuture<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitForCancellationFuture").finish()
    }
}

impl Future for WaitForCancellationFuture<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.get_mut().wait).poll(cx)
    }
}

/// Tells the compiler its contents may dangle: a reference inside is not
/// assumed valid for as long as a function argument holding it is in scope,
/// which the owned future needs because it drops the node its reference
/// points into while it is itself being dropped.
#[repr(transparent)]
struct MaybeDangling<T>(MaybeUninit<T>);

impl<T> MaybeDangling<T> {
    fn new(inner: T) -> Self {
        Self(MaybeUninit::new(inner))
    }

    fn get_mut(&mut self) -> &mut T {
        // SAFETY: the cell is initialised by `new` and dropped only by `Drop`.
        unsafe { self.0.assume_init_mut() }
    }
}

impl<T> Drop for MaybeDangling<T> {
    fn drop(&mut self) {
        // SAFETY: the cell is initialised and dropped only here.
        unsafe { self.0.assume_init_drop() };
    }
}

/// A future that resolves once the corresponding [`CancellationToken`] is
/// cancelled, and owns the token.
///
/// Created by [`CancellationToken::cancelled_owned`].
#[must_use = "futures do nothing unless polled"]
pub struct WaitForCancellationFutureOwned {
    // The wait borrows the node `token` keeps alive, with the borrow's
    // lifetime erased to `'static`. It is declared first so it drops first:
    // it must never be used or dropped after `token` is gone.
    wait: MaybeDangling<EventWait<'static>>,
    token: CancellationToken,
}

impl WaitForCancellationFutureOwned {
    pub(super) fn new(token: CancellationToken) -> Self {
        let event: *const Event = &token.node.cancelled;
        // SAFETY: the event lives inside the heap allocation `token` owns, so
        // it does not move when the future does and lives as long as `token`,
        // which is a field of the future that drops after `wait`.
        let wait = unsafe { (*event).wait() };
        Self {
            wait: MaybeDangling::new(wait),
            token,
        }
    }
}

impl fmt::Debug for WaitForCancellationFutureOwned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitForCancellationFutureOwned").finish()
    }
}

impl Future for WaitForCancellationFutureOwned {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(self.get_mut().wait.get_mut()).poll(cx)
    }
}
