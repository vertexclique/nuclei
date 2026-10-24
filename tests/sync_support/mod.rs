//! Helpers shared by the integration tests of the one-shot channel, the watch
//! channel and the cancellation token: a task that counts its wakes, a future
//! polled by hand, a parking `block_on`, and a drop counter.
//!
//! Each test file uses a subset, so unused items are expected.
#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

/// What a waker records: that it fired, and how often.
pub struct Notice {
    woken: AtomicBool,
    wakes: AtomicUsize,
}

impl Wake for Notice {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

/// A task context: hands out a waker and remembers whether it was woken.
pub struct Probe {
    notice: Arc<Notice>,
    waker: Waker,
}

impl Probe {
    pub fn new() -> Self {
        let notice = Arc::new(Notice {
            woken: AtomicBool::new(false),
            wakes: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&notice));
        Self { notice, waker }
    }

    /// Runs `f` with this task's context; the woken flag is cleared first.
    pub fn enter<R>(&mut self, f: impl FnOnce(&mut Context<'_>) -> R) -> R {
        self.notice.woken.store(false, Ordering::SeqCst);
        f(&mut Context::from_waker(&self.waker))
    }

    /// Whether the waker fired since the last `enter`.
    pub fn is_woken(&self) -> bool {
        self.notice.woken.load(Ordering::SeqCst)
    }

    /// How often the waker fired, ever.
    pub fn wakes(&self) -> usize {
        self.notice.wakes.load(Ordering::SeqCst)
    }

    /// How many handles to the waker's state exist beyond the probe's own:
    /// 1 when nothing else holds a clone of the waker.
    pub fn waker_ref_count(&self) -> usize {
        Arc::strong_count(&self.notice) - 1
    }
}

/// A future polled by hand under a [`Probe`].
pub struct Task<F> {
    probe: Probe,
    future: Pin<Box<F>>,
}

impl<F: Future> Task<F> {
    pub fn new(future: F) -> Self {
        Self {
            probe: Probe::new(),
            future: Box::pin(future),
        }
    }

    pub fn poll(&mut self) -> Poll<F::Output> {
        let future = self.future.as_mut();
        self.probe.enter(|cx| future.poll(cx))
    }

    /// Polls, expecting `Pending`.
    pub fn pending(&mut self) {
        assert!(self.poll().is_pending(), "the future completed early");
    }

    /// Polls, expecting `Ready`, and returns the output.
    pub fn ready(&mut self) -> F::Output {
        match self.poll() {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the future is still pending"),
        }
    }

    pub fn is_woken(&self) -> bool {
        self.probe.is_woken()
    }

    pub fn wakes(&self) -> usize {
        self.probe.wakes()
    }
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on this thread, parking between polls. A
/// watchdog fails the test instead of hanging it if nothing wakes the thread
/// for a minute.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        assert!(Instant::now() < deadline, "block_on: a wakeup was lost");
        std::thread::park_timeout(Duration::from_secs(1));
    }
}

/// Counts how many values were dropped.
#[derive(Clone, Default)]
pub struct Drops(pub Arc<AtomicUsize>);

impl Drops {
    pub fn new() -> Self {
        Self::default()
    }

    /// A value whose drop is counted here.
    pub fn value(&self) -> Counted {
        Counted(Arc::clone(&self.0))
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// A value that counts its own drop.
pub struct Counted(Arc<AtomicUsize>);

impl Clone for Counted {
    /// A clone is a new value with its own drop to count.
    fn clone(&self) -> Self {
        Counted(Arc::clone(&self.0))
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Waits (up to a deadline) for `condition`, calling `kovan::flush` between
/// looks: values the watch channel retires are freed in batches, some time
/// after the replacing call.
pub fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut backoff = Duration::from_micros(50);
    loop {
        kovan::flush();
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(20));
    }
}
