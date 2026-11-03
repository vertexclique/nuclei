//! The blocking frames of one thread, so nested ones never wait on each other.
//!
//! A task on a shard that calls a `blocking_*` method runs a nested
//! `block_on` frame, whose loop polls its own future and the shard's other
//! tasks. If one of those tasks blocks too, its frame sits on top of the
//! first, whose future cannot be polled until the top frame ends: a lock
//! granted to the first frame's wait would sit unused while the top frame
//! waits behind it.
//!
//! A wait that queues for a place in line (`blocking_in_line`) therefore
//! registers a record on a thread-local stack once it has answered `Pending`.
//! A frame that opens calls [`freeze`]: every record under it that is not
//! being polled withdraws its future from its queue (as a dropped one does,
//! passing on a grant or a nudge it holds) and is woken. Once its frame is on
//! top again its future is polled again and asks again, at the back.
//!
//! Invariants: a record is on the stack only between its first `Pending` and
//! the end of its call, the stack is last in, first out, and a record's
//! future is reached only by its own thread, never while it is polled.

#![allow(unsafe_code)]

use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll, Waker};

use super::internal::Withdraw;

thread_local! {
    /// The newest record of this thread, null when it has none.
    static HEAD: Cell<*const Header> = const { Cell::new(ptr::null()) };
}

/// A record without its future's type: what [`freeze`] reads.
#[repr(C)]
struct Header {
    /// Withdraws the future of the record this header heads.
    withdraw: unsafe fn(*const Header),
    /// The future was withdrawn and not polled since.
    frozen: Cell<bool>,
    /// The future is being polled.
    polling: Cell<bool>,
    /// On the stack.
    linked: Cell<bool>,
    /// The waker the future was last polled with, to wake it when frozen.
    waker: Cell<Option<Waker>>,
    /// The record under this one.
    prev: Cell<*const Header>,
}

/// A frame's future with its header; the header first, so a header pointer is
/// a record pointer.
#[repr(C)]
struct Record<F> {
    header: Header,
    future: UnsafeCell<F>,
}

/// Withdraws the future of the `Record<F>` that `header` heads.
///
/// # Safety
///
/// `header` heads a live `Record<F>` whose future is not being polled, and
/// which stays where it is.
unsafe fn withdraw_of<F: Withdraw>(header: *const Header) {
    // SAFETY: the caller's contract; `Record` is `repr(C)` with the header first.
    let record = unsafe { &*header.cast::<Record<F>>() };
    // SAFETY: the future is not polled meanwhile, and the record is pinned.
    unsafe { Pin::new_unchecked(&mut *record.future.get()) }.withdraw();
}

/// The future `block_on` polls for a record: the record's future, through it.
pub(super) struct Frame<'a, F> {
    record: &'a Record<F>,
}

impl<F: Future + Withdraw> Future for Frame<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let header = &self.record.header;
        header.frozen.set(false);
        /// Ends the poll, unwinding or not.
        struct Polling<'h>(&'h Header);
        impl Drop for Polling<'_> {
            fn drop(&mut self) {
                self.0.polling.set(false);
            }
        }
        header.polling.set(true);
        let polling = Polling(header);
        // SAFETY: the record is pinned on its caller's stack, and `polling`
        // keeps `freeze` from reaching the future meanwhile.
        let polled = unsafe { Pin::new_unchecked(&mut *self.record.future.get()) }.poll(cx);
        drop(polling);
        if polled.is_pending() {
            keep(header, cx.waker());
            link(header);
        }
        polled
    }
}

/// Keeps `waker` as the one to wake when the record is frozen, cloning only a
/// different one.
fn keep(header: &Header, waker: &Waker) {
    match header.waker.take() {
        Some(held) if held.will_wake(waker) => header.waker.set(Some(held)),
        _ => header.waker.set(Some(waker.clone())),
    }
}

/// Puts the record on the thread's stack, if it is not on it.
fn link(header: &Header) {
    if !header.linked.get() {
        header.prev.set(HEAD.get());
        HEAD.set(header);
        header.linked.set(true);
    }
}

/// Takes the record off the stack as its call ends, unwinding or not.
struct Unlink<'a>(&'a Header);

impl Drop for Unlink<'_> {
    fn drop(&mut self) {
        if self.0.linked.get() {
            debug_assert!(ptr::eq(HEAD.get(), self.0), "the frames end in order");
            HEAD.set(self.0.prev.get());
            self.0.linked.set(false);
        }
    }
}

/// Runs `run` on a frame for `future`, which is on the stack for as long as
/// it waits.
pub(super) fn in_line<F, R>(future: F, run: impl FnOnce(Frame<'_, F>) -> R) -> R
where
    F: Future + Withdraw,
{
    let record = Record {
        header: Header {
            withdraw: withdraw_of::<F>,
            frozen: Cell::new(false),
            polling: Cell::new(false),
            linked: Cell::new(false),
            waker: Cell::new(None),
            prev: Cell::new(ptr::null()),
        },
        future: UnsafeCell::new(future),
    };
    let _unlink = Unlink(&record.header);
    run(Frame { record: &record })
}

/// Whether this thread has a record to freeze: one load.
#[inline(always)]
pub(super) fn any() -> bool {
    !HEAD.get().is_null()
}

/// Withdraws every future on this thread's stack that is not being polled,
/// and wakes each, for a frame that opens over them.
#[cold]
#[inline(never)]
pub(super) fn freeze() {
    let mut node = HEAD.get();
    while !node.is_null() {
        // SAFETY: a linked record is live: it unlinks before it is dropped.
        let header = unsafe { &*node };
        if !header.frozen.get() && !header.polling.get() {
            header.frozen.set(true);
            // SAFETY: linked, so live and pinned; not polling.
            unsafe { (header.withdraw)(node) };
            if let Some(waker) = header.waker.take() {
                waker.wake_by_ref();
                header.waker.set(Some(waker));
            }
        }
        node = header.prev.get();
    }
}

/// How many records this thread has on its stack.
#[cfg(test)]
pub(crate) fn depth() -> usize {
    let mut count = 0;
    let mut node = HEAD.get();
    while !node.is_null() {
        count += 1;
        // SAFETY: as in `freeze`.
        node = unsafe { &*node }.prev.get();
    }
    count
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::sync::{Mutex, RwLock, Semaphore, mpsc};

    /// Runs `body` on a runtime of no workers, on a thread of its own, and
    /// fails when it does not end in time: a deadlock must fail the test, not
    /// hang it.
    fn within<R: Send + 'static>(body: impl FnOnce() -> R + Send + 'static) -> R {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(body());
        });
        result
            .recv_timeout(Duration::from_secs(30))
            .expect("a nested blocking wait never ended")
    }

    /// Yields until `started` reaches `count`: every task of the test has
    /// called its blocking method, whatever order the shard ran them in.
    async fn until(started: &AtomicUsize, count: usize) {
        while started.load(Ordering::SeqCst) < count {
            crate::task::yield_now().await;
        }
    }

    fn runtime() -> crate::Runtime {
        crate::Runtime::builder()
            .worker_threads(0)
            .build()
            .expect("a runtime")
    }

    #[test]
    fn nested_mutex_waits_never_wait_on_each_other_with_the_lock_free() {
        within(|| {
            runtime().block_on(async {
                let mutex = Arc::new(Mutex::new(0u32));
                let held = Arc::clone(&mutex).try_lock_owned().expect("free");
                let started = Arc::new(AtomicUsize::new(0));
                let wait = |m: Arc<Mutex<u32>>| {
                    let started = Arc::clone(&started);
                    crate::spawn(async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        *m.blocking_lock() += 1;
                    })
                };
                let outer = wait(Arc::clone(&mutex));
                let inner = wait(Arc::clone(&mutex));
                let gate = Arc::clone(&started);
                let release = crate::spawn(async move {
                    until(&gate, 2).await;
                    drop(held);
                });
                release.await;
                outer.await;
                inner.await;
                assert_eq!(*mutex.lock().await, 2);
            });
        });
    }

    #[test]
    fn a_frozen_wait_that_was_granted_passes_the_grant_to_the_frame_on_top() {
        within(|| {
            runtime().block_on(async {
                let (tx, mut rx) = mpsc::channel::<u32>(1);
                tx.try_send(0).expect("room");
                let started = Arc::new(AtomicUsize::new(0));
                let outer = {
                    let (tx, started) = (tx.clone(), Arc::clone(&started));
                    crate::spawn(async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        tx.blocking_send(10).expect("open");
                    })
                };
                // Runs inside the outer wait's frame: taking the value grants
                // the slot to the outer wait, then this task's own wait opens
                // a frame over it, and must get the slot the outer gave up.
                let inner = crate::spawn(async move {
                    until(&started, 1).await;
                    assert_eq!(rx.try_recv(), Ok(0));
                    tx.blocking_send(20).expect("open");
                    let first = rx.recv().await.expect("a value");
                    let second = rx.recv().await.expect("a value");
                    (first, second)
                });
                assert_eq!(inner.await, (20, 10));
                outer.await;
            });
        });
    }

    #[test]
    fn nested_semaphore_waits_never_wait_on_each_other() {
        within(|| {
            runtime().block_on(async {
                let sem = Arc::new(Semaphore::new(1));
                let held = Arc::clone(&sem).try_acquire_owned().expect("free");
                let started = Arc::new(AtomicUsize::new(0));
                let wait = |s: Arc<Semaphore>| {
                    let started = Arc::clone(&started);
                    crate::spawn(async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        drop(s.blocking_acquire().expect("open"));
                    })
                };
                let outer = wait(Arc::clone(&sem));
                let inner = wait(Arc::clone(&sem));
                let gate = Arc::clone(&started);
                let release = crate::spawn(async move {
                    until(&gate, 2).await;
                    drop(held);
                });
                release.await;
                outer.await;
                inner.await;
                assert_eq!(sem.available_permits(), 1);
            });
        });
    }

    #[test]
    fn nested_write_waits_never_wait_on_each_other() {
        within(|| {
            runtime().block_on(async {
                let lock = Arc::new(RwLock::new(0u32));
                let held = Arc::clone(&lock).write_owned().await;
                let started = Arc::new(AtomicUsize::new(0));
                let wait = |l: Arc<RwLock<u32>>| {
                    let started = Arc::clone(&started);
                    crate::spawn(async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        *l.blocking_write() += 1;
                    })
                };
                let outer = wait(Arc::clone(&lock));
                let inner = wait(Arc::clone(&lock));
                let gate = Arc::clone(&started);
                let release = crate::spawn(async move {
                    until(&gate, 2).await;
                    drop(held);
                });
                release.await;
                outer.await;
                inner.await;
                assert_eq!(*lock.read().await, 2);
            });
        });
    }

    #[test]
    fn nested_blocking_sends_never_wait_on_each_other() {
        within(|| {
            runtime().block_on(async {
                let (tx, mut rx) = mpsc::channel::<u32>(1);
                tx.try_send(0).expect("room");
                let started = Arc::new(AtomicUsize::new(0));
                let send = |tx: mpsc::Sender<u32>, value| {
                    let started = Arc::clone(&started);
                    crate::spawn(async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        tx.blocking_send(value).expect("open");
                    })
                };
                let outer = send(tx.clone(), 10);
                let inner = send(tx, 20);
                let gate = Arc::clone(&started);
                let receiver = crate::spawn(async move {
                    until(&gate, 2).await;
                    let mut got = Vec::new();
                    for _ in 0..3 {
                        got.push(rx.recv().await.expect("a value"));
                    }
                    got
                });
                outer.await;
                inner.await;
                let mut got = receiver.await;
                got.sort_unstable();
                assert_eq!(got, [0, 10, 20]);
            });
        });
    }

    /// A wait that wakes itself once, then panics.
    struct Panics(bool);

    impl Future for Panics {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                panic!("the frame's future panicked");
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    impl Withdraw for Panics {
        fn withdraw(self: Pin<&mut Self>) {}
    }

    #[test]
    fn the_stack_is_popped_after_a_panic_in_the_frames_future() {
        within(|| {
            runtime().block_on(async {
                crate::spawn(async {
                    let caught = std::panic::catch_unwind(|| {
                        crate::sync::internal::blocking_in_line(Panics(false));
                    });
                    assert!(caught.is_err());
                    assert_eq!(depth(), 0, "the record stayed on the stack");
                })
                .await;
            });
        });
    }
}
