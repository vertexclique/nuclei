//! Test-only helpers: a future polled by hand under a waker that counts
//! its wakes, and the thread-parking `block_on` for multi-threaded tests.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A future polled by hand, counting how often its waker fires.
pub(crate) struct Polled<F: Future> {
    future: Pin<Box<F>>,
    counter: Arc<Counter>,
    waker: Waker,
}

impl<F: Future> Polled<F> {
    pub(crate) fn new(future: F) -> Self {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        Self {
            future: Box::pin(future),
            counter,
            waker,
        }
    }

    pub(crate) fn poll(&mut self) -> Poll<F::Output> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    /// Polls, expecting `Pending`.
    pub(crate) fn pending(&mut self) {
        assert!(self.poll().is_pending(), "the future completed early");
    }

    /// Polls, expecting `Ready`, and returns the output.
    pub(crate) fn ready(&mut self) -> F::Output {
        match self.poll() {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the future is still pending"),
        }
    }

    pub(crate) fn wakes(&self) -> usize {
        self.counter.0.load(Ordering::SeqCst)
    }

    /// A clone of the counting waker, to poll some other future with.
    pub(crate) fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

/// Drives `future` to completion on this thread, parking between polls.
pub(crate) use super::internal::block_on;
