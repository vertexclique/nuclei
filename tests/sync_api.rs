//! The public surface of `nuclei::sync` that follows the established async
//! sync API: every constructor and method, every error path, the guards' `map`
//! family and the drop and cancel behaviour of the named futures.
//!
//! The waits are driven by hand under a waker that counts its wakes
//! (`Polled`), so a test can say exactly who was woken; the `blocking_*`
//! methods run on plain threads, as they must. The interleavings are the
//! loom models' business (`tests/sync_loom.rs`).
#![cfg(not(target_arch = "wasm32"))]

use std::error::Error;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nuclei::sync::{
    AcquireError, Barrier, MAX_BYPASS, MappedMutexGuard, Mutex, MutexGuard, Notify, OnceCell,
    OwnedMappedMutexGuard, OwnedMutexGuard, OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard,
    OwnedRwLockWriteGuard, RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard,
    Semaphore, SetError, SetOnce, SetOnceError, TryAcquireError, TryLockError,
};

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A future polled by hand under a waker that counts its wakes.
struct Polled<F: Future> {
    future: Pin<Box<F>>,
    counter: Arc<Counter>,
    waker: Waker,
}

impl<F: Future> Polled<F> {
    fn new(future: F) -> Self {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        Self {
            future: Box::pin(future),
            waker: Waker::from(Arc::clone(&counter)),
            counter,
        }
    }

    fn poll(&mut self) -> Poll<F::Output> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    /// Polls, expecting `Pending`.
    fn pending(&mut self) {
        assert!(self.poll().is_pending(), "the future completed early");
    }

    /// Polls, expecting `Ready`, and returns the output.
    fn ready(&mut self) -> F::Output {
        match self.poll() {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the future is still pending"),
        }
    }

    fn wakes(&self) -> usize {
        self.counter.0.load(Ordering::SeqCst)
    }
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on this thread, parking between polls.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

/// Runs `f` on a plain thread and returns what it returns.
fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().expect("the thread panicked")
}

// === the error types ===

#[test]
fn the_errors_display_and_debug_as_documented() {
    let mutex = Mutex::new(());
    let _held = mutex.try_lock().expect("free");
    let err: TryLockError = mutex.try_lock().unwrap_err();
    assert_eq!(err.to_string(), "operation would block");
    assert_eq!(format!("{err:?}"), "TryLockError(())");
    assert!(err.source().is_none());

    let sem = Semaphore::new(0);
    assert_eq!(
        sem.try_acquire().unwrap_err().to_string(),
        "no permits available"
    );
    sem.close();
    let closed = sem.try_acquire().unwrap_err();
    assert_eq!(closed, TryAcquireError::Closed);
    assert_eq!(closed.to_string(), "semaphore closed");
    assert_eq!(format!("{closed:?}"), "Closed");
    assert_eq!(format!("{:?}", TryAcquireError::NoPermits), "NoPermits");
    let acquire: AcquireError = block_on(sem.acquire()).unwrap_err();
    assert_eq!(acquire.to_string(), "semaphore closed");
    assert_eq!(format!("{acquire:?}"), "AcquireError(())");
    assert!(acquire.source().is_none() && closed.source().is_none());

    let cell = OnceCell::new();
    cell.set(1).expect("empty");
    let already = cell.set(2).unwrap_err();
    assert_eq!(already, SetError::AlreadyInitializedError(2));
    assert!(already.is_already_init_err() && !already.is_initializing_err());
    assert_eq!(already.to_string(), "AlreadyInitializedError");
    assert_eq!(format!("{already:?}"), "AlreadyInitializedError(2)");
    let initializing = SetError::InitializingError(3);
    assert!(initializing.is_initializing_err() && !initializing.is_already_init_err());
    assert_eq!(initializing.to_string(), "InitializingError");
    assert!(Error::source(&initializing).is_none());

    let once = SetOnce::new();
    once.set(1).expect("empty");
    let err = once.set(7).unwrap_err();
    assert_eq!(err, SetOnceError(7));
    assert_eq!(err.to_string(), "SetOnceError");
    assert_eq!(format!("{err:?}"), "SetOnceError(7)");
    let boxed: Box<dyn Error> = Box::new(err);
    assert_eq!(boxed.to_string(), "SetOnceError");
}

// === Mutex ===

#[test]
fn a_mutex_is_built_locked_and_unwrapped() {
    static COUNTER: Mutex<u32> = Mutex::const_new(1);
    *block_on(COUNTER.lock()) += 1;
    assert_eq!(*COUNTER.try_lock().expect("free"), 2);

    let mut mutex = Mutex::new(String::from("a"));
    mutex.get_mut().push('b');
    assert_eq!(mutex.try_lock().expect("free").as_str(), "ab");
    assert_eq!(mutex.into_inner(), "ab");
    let from: Mutex<u8> = 5.into();
    assert_eq!(from.into_inner(), 5);
    assert_eq!(Mutex::<u8>::default().into_inner(), 0);
    let mutex = Mutex::new(7);
    assert_eq!(format!("{mutex:?}"), "Mutex { data: 7 }");
    let guard = mutex.try_lock().expect("free");
    assert_eq!(format!("{mutex:?}"), "Mutex { data: <locked> }");
    assert_eq!(format!("{guard} {guard:?}"), "7 7");
}

#[test]
fn owned_mutex_calls_take_the_arc_by_value() {
    let mutex = Arc::new(Mutex::new(1));
    let guard = block_on(Arc::clone(&mutex).lock_owned());
    assert!(Arc::clone(&mutex).try_lock_owned().is_err());
    assert!(Arc::ptr_eq(OwnedMutexGuard::mutex(&guard), &mutex));
    let mut waiting = Polled::new(Arc::clone(&mutex).lock_owned());
    waiting.pending();
    drop(guard);
    assert_eq!(waiting.wakes(), 1);
    let mut guard = waiting.ready();
    *guard += 1;
    assert_eq!(format!("{guard} {guard:?}"), "2 2");
    drop(guard);
    let owned = Arc::clone(&mutex).try_lock_owned().expect("free again");
    assert_eq!(*owned, 2);
}

#[test]
fn the_blocking_mutex_calls_wait_on_a_plain_thread() {
    let mutex = Arc::new(Mutex::new(0));
    let held = mutex.blocking_lock();
    let worker = {
        let mutex = Arc::clone(&mutex);
        std::thread::spawn(move || {
            *mutex.blocking_lock() += 1;
            *Arc::clone(&mutex).blocking_lock_owned() += 10;
        })
    };
    drop(held);
    worker.join().expect("worker");
    assert_eq!(*mutex.try_lock().expect("free"), 11);
}

#[test]
fn a_mutex_guard_maps_to_a_component_and_keeps_the_lock() {
    let mutex = Arc::new(Mutex::new((1u32, String::from("x"))));
    let mut number = MutexGuard::map(block_on(mutex.lock()), |pair| &mut pair.0);
    *number += 1;
    assert_eq!(format!("{number} {number:?}"), "2 2");
    assert!(mutex.try_lock().is_err(), "the mapped guard holds the lock");
    let mut waiter = Polled::new(mutex.lock());
    waiter.pending();
    drop(number);
    assert_eq!(waiter.wakes(), 1, "dropping the mapped guard unlocks");
    assert_eq!(waiter.ready().0, 2);

    let guard = MutexGuard::try_map(block_on(mutex.lock()), |pair| pair.1.get_mut(5..))
        .expect_err("nothing there");
    assert!(mutex.try_lock().is_err(), "a declined map keeps the lock");
    let mut text = MutexGuard::try_map(guard, |pair| Some(&mut pair.1)).expect("found");
    text.push('y');
    let text = MappedMutexGuard::try_map(text, |s| s.get_mut(5..)).expect_err("short");
    let mut tail = MappedMutexGuard::map(text, |s| s.as_mut_str());
    tail.make_ascii_uppercase();
    drop(tail);
    assert_eq!(mutex.try_lock().expect("free").1, "XY");

    let guard = block_on(mutex.lock());
    assert!(std::ptr::eq(MutexGuard::mutex(&guard), &*mutex));
}

#[test]
fn an_owned_mutex_guard_maps_and_keeps_the_arc_alive() {
    let mutex = Arc::new(Mutex::new((1u32, 2u32)));
    let guard = block_on(Arc::clone(&mutex).lock_owned());
    let guard = OwnedMutexGuard::try_map(guard, |pair| (pair.0 > 5).then_some(&mut pair.0))
        .expect_err("one is small");
    let mut second = OwnedMutexGuard::map(guard, |pair| &mut pair.1);
    *second += 5;
    let second = OwnedMappedMutexGuard::try_map(second, |n| (*n > 100).then_some(n))
        .expect_err("seven is small");
    let mut whole = OwnedMappedMutexGuard::map(second, |n| n);
    *whole += 1;
    assert_eq!(format!("{whole} {whole:?}"), "8 8");
    drop(mutex);
    assert_eq!(*whole, 8, "the guard keeps the mutex alive");
    drop(whole);
}

#[test]
fn a_dropped_lock_future_hands_its_lock_on() {
    let mutex = Mutex::new(());
    let held = mutex.try_lock().expect("free");
    let mut first = Polled::new(mutex.lock());
    first.pending();
    let mut second = Polled::new(mutex.lock());
    second.pending();
    drop(held);
    assert_eq!((first.wakes(), second.wakes()), (1, 0));
    drop(first);
    assert_eq!(second.wakes(), 1, "the dropped wait woke the next one");
    drop(second.ready());
    assert!(mutex.try_lock().is_ok());

    let arc = Arc::new(Mutex::new(()));
    let held = arc.try_lock().expect("free");
    let mut owned = Polled::new(Arc::clone(&arc).lock_owned());
    owned.pending();
    let mut next = Polled::new(arc.lock());
    next.pending();
    drop(held);
    drop(owned);
    assert_eq!(next.wakes(), 1);
    drop(next.ready());
    assert!(arc.try_lock().is_ok(), "nothing was left held");
}

#[path = "sync_api/cells.rs"]
mod cells;
#[path = "sync_api/notify.rs"]
mod notify;
#[path = "sync_api/rwlock.rs"]
mod rwlock;
#[path = "sync_api/semaphore.rs"]
mod semaphore;
