//! A one-shot event: set once, and every wait completes from then on.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::internal::{Ordering, UnsafeCell};
use super::list::List;
use super::queue::{Arrivals, FIRST_BIT, Policy, QUEUED, Step, WaitQueue};
use super::waiter::{Cancel, Wait, WakeList};

const SET: usize = 1 << FIRST_BIT;

/// An event that is set once and stays set.
///
/// [`wait`](Self::wait) completes as soon as the event is set, whether
/// that happened before or after the wait began; [`set`](Self::set) wakes
/// every waiter. Checking is one atomic load.
///
/// ```
/// use nuclei::sync::Event;
///
/// let ready = Event::new();
/// assert!(!ready.is_set());
/// ready.set();
/// assert!(ready.is_set());
/// ```
pub struct Event {
    queue: WaitQueue,
    waiting: UnsafeCell<List>,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl Event {
    loom_const_fn! {
        /// An event that is not set.
        pub fn new() -> Self {
            Self::with_state(0)
        }
    }

    loom_const_fn! {
        /// An event that is already set when `set` is true.
        pub(super) fn new_set(set: bool) -> Self {
            Self::with_state(if set { SET } else { 0 })
        }
    }

    loom_const_fn! {
        fn with_state(state: usize) -> Self {
            Self { queue: WaitQueue::new(state), waiting: UnsafeCell::new(List::new()) }
        }
    }

    /// Sets the event and wakes every waiter. Setting it again does
    /// nothing.
    pub fn set(&self) {
        self.transition(|state| {
            if state & SET != 0 {
                Step::Keep
            } else if state & QUEUED != 0 {
                Step::Drain(state | SET)
            } else {
                Step::Set(state | SET)
            }
        });
    }

    /// Whether the event is set.
    pub fn is_set(&self) -> bool {
        self.queue.state.load(Ordering::Acquire) & SET != 0
    }

    /// A future that completes once the event is set.
    pub fn wait(&self) -> EventWait<'_> {
        EventWait {
            event: self,
            wait: Wait::new(),
        }
    }
}

impl Policy for Event {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, _wake_front: bool) -> usize {
        let pool = &self.queue;
        let set = self.is_set();
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            waiting.absorb(arrivals, pool);
            waiting.tidy(&self.queue);
            while set && let Some(node) = waiting.front(pool) {
                waiting.pop_front();
                woken.grant(node);
            }
            if waiting.front(pool).is_none() {
                QUEUED
            } else {
                0
            }
        })
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let pool = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).clear(pool) });
    }
}

impl Default for Event {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("set", &self.is_set())
            .finish()
    }
}

/// The future [`Event::wait`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct EventWait<'a> {
    event: &'a Event,
    wait: Wait,
}

impl Future for EventWait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let event = this.event;
        if this.wait.is_queued() {
            return this.wait.poll(&event.queue, Some(cx.waker())).map(drop);
        }
        if event.is_set() {
            return Poll::Ready(());
        }
        event.enqueue(&mut this.wait, 0, cx.waker());
        this.wait.poll(&event.queue, None).map(drop)
    }
}

impl Drop for EventWait<'_> {
    fn drop(&mut self) {
        if let Cancel::Withdrawn(_) = self.wait.cancel(&self.event.queue) {
            self.event.withdrawn(false);
        }
    }
}

impl fmt::Debug for EventWait<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventWait").finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::Arc;

    #[test]
    fn setting_wakes_every_waiter_and_later_waits_pass() {
        let event = Event::new();
        let mut waiters: Vec<_> = (0..3).map(|_| Polled::new(event.wait())).collect();
        for waiter in &mut waiters {
            waiter.pending();
        }
        drop(waiters.pop());
        event.set();
        event.set();
        for waiter in &mut waiters {
            assert_eq!(waiter.wakes(), 1);
            waiter.ready();
        }
        Polled::new(event.wait()).ready();
    }

    #[test]
    fn a_set_from_another_thread_is_seen() {
        let event = Arc::new(Event::new());
        let setter = Arc::clone(&event);
        let thread = std::thread::spawn(move || setter.set());
        block_on(event.wait());
        thread.join().expect("setter");
        assert!(event.is_set());
    }
}
