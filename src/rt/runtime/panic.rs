//! A runtime that shuts down when one of its tasks panics
//! ([`UnhandledPanic::ShutdownRuntime`](crate::runtime::UnhandledPanic)):
//! the panic sets the runtime's flag, stops its workers and wakes every
//! `block_on` of the runtime, each of which then panics, as every later one
//! does. A runtime that ignores panics pays nothing for this: its tasks
//! carry no link back to it, and its `block_on` runs the caller's future
//! as it is.

use core::future::Future;
use core::pin::pin;
use core::task::Poll;
use std::sync::atomic::Ordering;

use super::Inner;

impl Inner {
    /// A task of this runtime panicked, and the runtime shuts down on that.
    pub(crate) fn shut_down_on_panic(&self) {
        // The flag before the wake: a `block_on` reads it once its wait is
        // made, so it either sees the flag or hears the wake.
        if !self.panicked.swap(true, Ordering::AcqRel) {
            self.workers.stop_all();
            self.on_panic.notify_waiters();
        }
    }
}

/// `future`, which panics instead once a task of `inner` panicked.
pub(super) async fn watched<F: Future>(inner: &Inner, future: F) -> F::Output {
    let mut notified = pin!(inner.on_panic.notified());
    let mut future = pin!(future);
    core::future::poll_fn(|cx| {
        if inner.panicked.load(Ordering::Acquire) {
            shut_down();
        }
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(output);
        }
        if notified.as_mut().poll(cx).is_ready() {
            shut_down();
        }
        Poll::Pending
    })
    .await
}

#[cold]
#[expect(
    clippy::panic,
    reason = "`UnhandledPanic::ShutdownRuntime`: every `block_on` of the runtime panics, as documented"
)]
fn shut_down() -> ! {
    panic!("the runtime shut down because one of its tasks panicked")
}
