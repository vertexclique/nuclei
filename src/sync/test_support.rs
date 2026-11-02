//! Test-only helpers: a future polled by hand under a waker that counts
//! its wakes, a waker that runs code at its first clone or wake, a poll with
//! its budget spent, and the thread-parking `block_on` for multi-threaded
//! tests.

#![allow(unsafe_code)]

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// A waker that runs `on_clone` at its first clone and `on_wake` at its
/// first wake by reference: what another thread could do at those
/// moments of a poll.
struct Hook {
    on_clone: Cell<Option<Box<dyn FnOnce()>>>,
    on_wake: Cell<Option<Box<dyn FnOnce()>>>,
}

static HOOK: RawWakerVTable =
    RawWakerVTable::new(hook_clone, hook_wake, hook_wake_by_ref, hook_drop);

// Each of the four functions below is the vtable entry of a waker built
// from `Rc::into_raw` of a `Hook`, which is their contract; the tests use
// it on their own thread only.
unsafe fn hook_clone(data: *const ()) -> RawWaker {
    // SAFETY: `data` came from `Rc::into_raw` of a `Hook` and is live.
    let hook = unsafe { &*(data as *const Hook) };
    if let Some(run) = hook.on_clone.take() {
        run();
    }
    // SAFETY: `data` is a live `Rc` pointer; this adds the clone's count.
    unsafe { Rc::increment_strong_count(data as *const Hook) };
    RawWaker::new(data, &HOOK)
}

unsafe fn hook_wake(data: *const ()) {
    // SAFETY: `data` is a live `Rc` pointer, whose count this consumes.
    unsafe { hook_wake_by_ref(data) };
    // SAFETY: as above.
    unsafe { hook_drop(data) };
}

unsafe fn hook_wake_by_ref(data: *const ()) {
    // SAFETY: `data` came from `Rc::into_raw` of a `Hook` and is live.
    let hook = unsafe { &*(data as *const Hook) };
    if let Some(run) = hook.on_wake.take() {
        run();
    }
}

unsafe fn hook_drop(data: *const ()) {
    // SAFETY: consumes the count this waker owns.
    drop(unsafe { Rc::from_raw(data as *const Hook) });
}

pub(crate) fn hooked(
    on_clone: Option<Box<dyn FnOnce()>>,
    on_wake: Option<Box<dyn FnOnce()>>,
) -> Waker {
    let hook = Rc::new(Hook {
        on_clone: Cell::new(on_clone),
        on_wake: Cell::new(on_wake),
    });
    let data = Rc::into_raw(hook) as *const ();
    // SAFETY: the vtable's contract is met by a pointer from `Rc::into_raw`.
    unsafe { Waker::from_raw(RawWaker::new(data, &HOOK)) }
}

static DROP_PANICS: RawWakerVTable = RawWakerVTable::new(
    drop_panics_clone,
    drop_panics_noop,
    drop_panics_noop,
    drop_panics_drop,
);

// The four functions below are the vtable of a waker built by `drop_panics`,
// whose data is a leaked `AtomicBool`.
unsafe fn drop_panics_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &DROP_PANICS)
}

unsafe fn drop_panics_noop(_: *const ()) {}

unsafe fn drop_panics_drop(data: *const ()) {
    // SAFETY: `data` is the leaked flag of `drop_panics`, live for good.
    let armed = unsafe { &*(data as *const AtomicBool) };
    if armed.swap(false, Ordering::SeqCst) {
        panic!("a waker's drop panicked");
    }
}

/// A waker whose first drop, of it or of any clone, panics: what a task's
/// waker does when releasing it runs user code that fails. It wakes nobody.
pub(crate) fn drop_panics() -> Waker {
    let armed: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(true)));
    // SAFETY: the vtable's contract is met by the leaked flag it reads.
    unsafe {
        Waker::from_raw(RawWaker::new(
            armed as *const AtomicBool as *const (),
            &DROP_PANICS,
        ))
    }
}

/// Runs `poll` inside a poll whose budget is spent.
pub(crate) fn with_budget_spent<R>(poll: impl FnOnce() -> R) -> R {
    let saved = crate::coop::enter();
    while crate::coop::try_spend() {}
    let result = poll();
    crate::coop::leave(saved);
    result
}

/// Drives `future` to completion on this thread, parking between polls.
pub(crate) use super::internal::block_on;
