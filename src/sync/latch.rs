//! A count-down latch: waits complete once a count reaches zero.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::event::{Event, EventWait};
use super::internal::{AtomicUsize, Ordering};

/// A latch that opens when its count reaches zero, and stays open.
///
/// ```
/// use nuclei::sync::Latch;
///
/// let done = Latch::new(2);
/// done.count_down();
/// assert!(!done.is_set());
/// done.count_down();
/// assert!(done.is_set());
/// ```
pub struct Latch {
    count: AtomicUsize,
    open: Event,
}

impl Latch {
    loom_const_fn! {
        /// A latch that opens after `count` calls to
        /// [`count_down`](Self::count_down); open at once when `count` is
        /// zero.
        pub fn new(count: usize) -> Self {
            Self { count: AtomicUsize::new(count), open: Event::new_set(count == 0) }
        }
    }

    /// Lowers the count by one, opening the latch when it reaches zero.
    /// Does nothing once the latch is open.
    pub fn count_down(&self) {
        let mut count = self.count.load(Ordering::Relaxed);
        while count != 0 {
            match self.count.compare_exchange_weak(
                count,
                count - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(1) => return self.open.set(),
                Ok(_) => return,
                Err(actual) => count = actual,
            }
        }
    }

    /// The count still to go.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Whether the latch is open.
    pub fn is_set(&self) -> bool {
        self.open.is_set()
    }

    /// A future that completes once the latch is open.
    pub fn wait(&self) -> LatchWait<'_> {
        LatchWait(self.open.wait())
    }
}

impl fmt::Debug for Latch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Latch")
            .field("count", &self.count())
            .finish()
    }
}

/// The future [`Latch::wait`] returns.
#[must_use = "futures do nothing unless polled"]
#[derive(Debug)]
pub struct LatchWait<'a>(EventWait<'a>);

impl Future for LatchWait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.get_mut().0).poll(cx)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::Polled;

    #[test]
    fn the_latch_opens_at_zero_and_stays_open() {
        let latch = Latch::new(2);
        let mut waiting = Polled::new(latch.wait());
        waiting.pending();
        latch.count_down();
        assert_eq!(waiting.wakes(), 0);
        latch.count_down();
        latch.count_down();
        assert_eq!(latch.count(), 0);
        waiting.ready();
        Polled::new(latch.wait()).ready();
        Polled::new(Latch::new(0).wait()).ready();
    }
}
