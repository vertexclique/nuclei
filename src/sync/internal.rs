//! The atomics and locks the synchronization primitives build on, and how a
//! thread waits on one.
//!
//! Everything here has two implementations selected by `cfg(loom)`. A
//! normal build takes `std::sync`; a `--cfg loom` build takes
//! `loom::sync`, whose instrumented mocks are the only accesses loom
//! can see. A `std::sync::atomic::AtomicPtr` is invisible to the model
//! checker, so anything that wants to be model-checked imports from
//! here rather than from `std` directly.
//!
//! The loom mocks are only usable inside a `loom::model` call, so
//! `cfg(loom)` is a model-checking build and nothing else.
//!
//! # Why the wrapper exists
//!
//! `Mutex` is a thin newtype rather than a re-export because `std`
//! poisons a lock whose holder panicked and hands back a `LockResult`,
//! while nothing here has use for poisoning: a panic in one thread must
//! not convert every later lock acquisition in every other thread into a
//! second panic, which is how a single failing assertion turns into a torn
//! shutdown. The wrapper recovers the guard with
//! [`std::sync::PoisonError::into_inner`] and hands it back directly, so a
//! caller sees the lock's contents exactly as the panicking thread left
//! them and deals with it as ordinary state.
//!
//! No primitive of this module takes it, so on Unix, Windows and WASI it is
//! compiled for tests alone. What has to exclude or wait uses the lock-free
//! primitives of [`crate::sync`], and a thread that has to wait on one of
//! those waits through [`block_on`] or [`wait_here`], below.
//!
//! [`UnsafeCell`] lives here for the same reason as everything else: a
//! `std::cell::UnsafeCell` is invisible to loom, so state that is handed
//! from one thread to another without a lock goes through the wrapper here
//! instead.

// vertexia: the whole crate-private surface of this module stays declared;
// names no primitive imports yet are allowed unused, and any that stay unused
// are trimmed.
/// The shared state made per operation, in a block of the thread's cache.
#[cfg(not(all(loom, test)))]
pub(crate) use crate::mem::BlockArc;
#[cfg(all(loom, test))]
#[allow(unused_imports)]
pub(crate) use loom::sync::Arc;
/// The shared state made per operation: loom's `Arc` in the models.
#[cfg(all(loom, test))]
pub(crate) use loom::sync::Arc as BlockArc;
#[cfg(not(all(loom, test)))]
#[allow(unused_imports)]
pub(crate) use std::sync::Arc;

#[cfg(all(loom, test))]
#[allow(unused_imports)]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(not(all(loom, test)))]
#[allow(unused_imports)]
pub(crate) use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

/// Take the pointer out of `atomic`, leaving null, through `&mut`: no other
/// thread can reach the atomic, so this is a plain read and write and not a
/// locked instruction. What a `Drop` of a cell built on an `AtomicPtr` uses,
/// since a `swap` there costs a locked exchange for nothing.
///
/// Under loom it is still a `swap`: a value that `std::sync::Arc` hands from
/// one thread to the one that drops it reaches that thread by an edge loom
/// cannot see, and loom would take the plain access for a race. The exclusive access itself is the borrow checker's to prove, not
/// loom's.
#[inline(always)]
pub(crate) fn take_ptr<T>(atomic: &mut AtomicPtr<T>) -> *mut T {
    #[cfg(not(all(loom, test)))]
    {
        std::mem::replace(atomic.get_mut(), std::ptr::null_mut())
    }
    #[cfg(all(loom, test))]
    {
        atomic.swap(std::ptr::null_mut(), Ordering::AcqRel)
    }
}

/// The point between the store and the load of one side of a handshake
/// built from `SeqCst` accesses, for loom.
///
/// Two threads that each store one word and then load the other's ("I am
/// here" against "it is gone") rely on the one total order C11 gives every
/// `SeqCst` access of a program: whichever store comes second in that order,
/// the load after it sees the other store, so at least one side sees the
/// other. loom does not model that order for accesses, only for
/// `fence(SeqCst)`, so the handshake fails under loom though it holds in
/// every real execution. Each side calls this between its store and its
/// load. Under `--cfg loom` it is a `SeqCst` fence, which gives the model
/// the order the accesses have; in an ordinary build it is nothing, and the
/// accesses carry the order themselves.
#[inline(always)]
pub(crate) fn seq_cst_handshake() {
    #[cfg(all(loom, test))]
    fence(Ordering::SeqCst);
}

#[cfg(any(test, not(any(unix, windows, target_os = "wasi"))))]
pub(crate) use os_mutex::Mutex;

/// The one blocking lock, kept apart so that what compiles it in is said
/// in one place: tests, and a target that is none of Unix, Windows or WASI.
#[cfg(any(test, not(any(unix, windows, target_os = "wasi"))))]
// A loom build compiles the tests that use it out.
#[cfg_attr(all(loom, test), allow(dead_code))]
mod os_mutex {
    #[cfg(all(loom, test))]
    use loom::sync as imp;
    #[cfg(not(all(loom, test)))]
    use std::sync as imp;

    use imp::MutexGuard;
    use std::sync::PoisonError;

    /// A mutex that hands back its guard rather than a `LockResult`.
    ///
    /// See the module docs for why poisoning is absorbed rather than
    /// propagated.
    #[derive(Default)]
    pub(crate) struct Mutex<T>(imp::Mutex<T>);

    impl<T> Mutex<T> {
        /// A new mutex holding `value`.
        #[cfg(not(all(loom, test)))]
        pub(crate) const fn new(value: T) -> Self {
            Self(imp::Mutex::new(value))
        }

        /// A new mutex holding `value`.
        ///
        /// Not `const` under loom: the mock allocates its own state.
        #[cfg(all(loom, test))]
        pub(crate) fn new(value: T) -> Self {
            Self(imp::Mutex::new(value))
        }

        /// Consume the mutex and return what it held.
        #[allow(dead_code)]
        pub(crate) fn into_inner(self) -> T {
            self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl<T> Mutex<T> {
        /// Lock, blocking until the mutex is available.
        pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Borrow the contents mutably. Exclusive access is proven by
        /// `&mut self`, so this takes no lock.
        #[allow(dead_code)]
        pub(crate) fn get_mut(&mut self) -> &mut T {
            self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl<T: std::fmt::Debug> std::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }
}

/// Interior mutability for state that exactly one thread touches at a
/// time, such as a slot that is filled before it is published.
///
/// Under loom the cell is instrumented and an access that is not ordered
/// before or after every other access fails the model; in a normal build
/// it is `std::cell::UnsafeCell` behind the same `with`/`with_mut` shape,
/// so the code the model checks is the code that ships.
#[cfg(not(all(loom, test)))]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

#[cfg(not(all(loom, test)))]
impl<T> UnsafeCell<T> {
    /// A new cell holding `value`.
    pub(crate) const fn new(value: T) -> Self {
        Self(std::cell::UnsafeCell::new(value))
    }

    /// Run `f` on a shared pointer to the contents.
    ///
    pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    /// Run `f` on an exclusive pointer to the contents.
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}

#[cfg(all(loom, test))]
pub(crate) use loom::cell::UnsafeCell;

/// Wakes a thread parked in [`block_on`].
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Unpark(pub(crate) std::thread::Thread);

#[cfg(not(target_arch = "wasm32"))]
impl std::task::Wake for Unpark {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive `future` to completion on this thread: poll it, and while it is
/// pending, park the thread until its waker fires. This is how a thread that
/// may wait waits on a [`crate::sync`] primitive. A wake that comes before
/// the park is kept by the thread, so the park returns at once and nothing is
/// lost.
#[cfg(not(any(all(loom, test), target_arch = "wasm32")))]
pub(crate) fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll};

    let waker = std::task::Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    loop {
        // A thread parked in a wait runs nothing else, so its polls are unbudgeted: a spent
        // budget of the poll around it (a task's) must not refuse every poll and spin.
        if let Poll::Ready(output) = crate::coop::outside(|| future.as_mut().poll(&mut cx)) {
            return output;
        }
        std::thread::park();
    }
}

/// Under loom a wait parks the model's thread on a tracked notification,
/// so a wake that never comes parks it for good and fails the model as a
/// deadlock.
#[cfg(all(loom, test))]
pub(crate) fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll};

    struct Notify(loom::sync::Notify);

    impl std::task::Wake for Notify {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.notify();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.notify();
        }
    }

    let park = std::sync::Arc::new(Notify(loom::sync::Notify::new()));
    let waker = std::task::Waker::from(std::sync::Arc::clone(&park));
    let mut cx = Context::from_waker(&waker);
    let mut future = core::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = crate::coop::outside(|| future.as_mut().poll(&mut cx)) {
            return output;
        }
        park.0.wait();
    }
}

/// `future`'s output, waited for on this thread: the blocking calls' way to
/// take a [`crate::sync`] primitive on every target.
///
/// On `wasm32` the one thread cannot wait for another, so only a future that
/// is ready at once completes; one that is not would wait for this same
/// thread, which already holds what it asks for, and fails instead.
pub(crate) fn wait_here<F: core::future::Future>(future: F) -> std::io::Result<F::Output> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Ok(block_on(future))
    }
    #[cfg(target_arch = "wasm32")]
    {
        use core::task::{Context, Poll};

        let mut future = core::pin::pin!(future);
        // Unbudgeted: a spent budget would refuse a free lock and read as a wait on this thread.
        match crate::coop::outside(|| {
            future
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
        }) {
            Poll::Ready(output) => Ok(output),
            // Nothing else runs on this target, so whoever holds it is this
            // thread, further up its own stack.
            Poll::Pending => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "this call waits for something its own thread holds; finish that first",
            )),
        }
    }
}

/// Runs `f`, user code reached while unwinding from an earlier panic (the
/// rest of a wake list, a give-back in a guard): a second panic there would
/// abort the process, so it is caught and its payload dropped, the panic
/// hook having reported it already.
pub(crate) fn during_unwind(f: impl FnOnce()) {
    if let Some(panic) = crate::rt::caught::catch(f) {
        crate::rt::caught::drop_payload(panic);
    }
}

/// Runs a give-back through [`during_unwind`] when dropped, which only an
/// unwind does: the normal path ends in [`disarm`](Self::disarm), which costs
/// nothing. For what was taken before user code that may panic runs.
#[must_use = "dropped at once, it runs its give-back"]
pub(crate) struct OnUnwind<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnUnwind<F> {
    #[inline(always)]
    pub(crate) fn new(undo: F) -> Self {
        Self(Some(undo))
    }

    /// Ends the guard without running `undo`, which then is never dropped
    /// either: it holds borrows and `Copy` values alone.
    #[inline(always)]
    pub(crate) fn disarm(self) {
        core::mem::forget(self);
    }
}

impl<F: FnOnce()> Drop for OnUnwind<F> {
    #[cold]
    fn drop(&mut self) {
        if let Some(undo) = self.0.take() {
            during_unwind(undo);
        }
    }
}

/// A wait that holds a place in a queue, which a frame that opens over it can
/// make it give up (see `frames`).
pub(crate) trait Withdraw {
    /// Leaves every queue the future holds a place in, as dropping it does
    /// (passing on a grant or a nudge it holds), and leaves it fresh: its
    /// next poll starts over, at the back.
    fn withdraw(self: core::pin::Pin<&mut Self>);
}

/// Withdraws every wait under a frame that is about to open on this thread
/// (see `frames`): one load when there is none.
#[inline(always)]
pub(crate) fn freeze_frames() {
    #[cfg(not(any(all(loom, test), target_arch = "wasm32")))]
    if super::frames::any() {
        super::frames::freeze();
    }
}

/// [`blocking`] for a wait that queues: on a shard, the wait gives up its
/// place in line whenever a frame opens over it and asks again, at the back,
/// once its frame is on top again, so nested blocking waits never wait on
/// each other with the lock free.
///
/// # Panics
///
/// On `wasm32`, when the future is not ready at once: see [`wait_here`].
#[expect(
    clippy::panic,
    reason = "documented under `# Panics`: a wait that would park the only thread"
)]
pub(crate) fn blocking_in_line<F: core::future::Future + Withdraw>(future: F) -> F::Output {
    #[cfg(not(any(all(loom, test), target_arch = "wasm32")))]
    {
        if crate::rt::context::current(|current| current.is_some()) {
            return super::frames::in_line(future, |frame| crate::runtime::block_on(frame));
        }
        if let Some(shard) = crate::rt::context::own_shard_if_made() {
            let _entered = crate::rt::context::enter(&shard);
            return super::frames::in_line(future, |frame| shard.block_on_then_leave(frame));
        }
    }
    match wait_here(future) {
        Ok(output) => output,
        Err(stuck) => panic!("{stuck}"),
    }
}

/// `future`'s output, waited for on this thread: what every `blocking_*`
/// method runs, so they all behave the same way.
///
/// On a thread that runs a nuclei shard (inside one of its tasks), or whose poll lost its
/// worker's shard to the handoff and can start its own, it runs a nested `block_on` on the shard
/// it runs or its own, which keeps running that shard's other tasks, timers and IO (the thread's
/// bound tasks among them) until the future completes. On a thread that runs none but made its
/// own shard, it runs that shard until the future completes. On any other thread it parks the
/// thread (a system call) until the future's waker fires, so it must not run on a thread
/// another executor needs for other tasks.
///
/// # Panics
///
/// On `wasm32`, when the future is not ready at once: see [`wait_here`].
#[expect(
    clippy::panic,
    reason = "documented under `# Panics`: a wait that would park the only thread"
)]
pub(crate) fn blocking<F: core::future::Future>(future: F) -> F::Output {
    #[cfg(not(any(all(loom, test), target_arch = "wasm32")))]
    {
        if crate::rt::context::current(|current| current.is_some())
            || (crate::rt::context::lost_a_shard() && crate::rt::context::own_shard().is_ok())
        {
            return crate::runtime::block_on(future);
        }
        // Only this thread runs its own shard: what waits on it (an edge,
        // a completion, a request posted to it) comes only if it runs.
        if let Some(shard) = crate::rt::context::own_shard_if_made() {
            let _entered = crate::rt::context::enter(&shard);
            return shard.block_on_then_leave(future);
        }
    }
    match wait_here(future) {
        Ok(output) => output,
        Err(stuck) => panic!("{stuck}"),
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    #[allow(unsafe_code)]
    fn the_unsafe_cell_shim_round_trips() {
        let cell = UnsafeCell::new(vec![1u32, 2, 3]);
        // SAFETY: single-threaded test, no other access overlaps these.
        cell.with_mut(|ptr| unsafe { (*ptr).push(4) });
        let read = cell.with(|ptr| unsafe { (*ptr).clone() });
        assert_eq!(read, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_lock_whose_holder_panicked_still_hands_back_its_contents() {
        let mutex = Arc::new(Mutex::new(vec![1u32, 2, 3]));
        let poisoner = Arc::clone(&mutex);
        let panicked = std::thread::spawn(move || {
            let mut guard = poisoner.lock();
            guard.push(4);
            panic!("the holder dies mid-update");
        })
        .join();
        assert!(panicked.is_err(), "the probe needs the thread to panic");

        assert_eq!(
            *mutex.lock(),
            vec![1, 2, 3, 4],
            "poisoning must not hide what the panicking thread had already written",
        );
    }

    #[test]
    fn a_wait_inside_a_spent_budget_still_polls_its_future() {
        let (done, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let previous = crate::coop::enter();
            while crate::coop::try_spend() {}
            let out = block_on(core::future::poll_fn(|cx| {
                if crate::coop::spend(cx.waker()) {
                    core::task::Poll::Ready(7)
                } else {
                    core::task::Poll::Pending
                }
            }));
            crate::coop::leave(previous);
            let _ = done.send(out);
        });
        assert_eq!(
            answer.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(7),
            "block_on spun on the spent budget of the poll around it"
        );
    }
}
