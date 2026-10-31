//! Nuclei's runtime-independent synchronization primitives: locks, a
//! semaphore, events, a barrier and once-cells that never block a thread.
//!
//! Every wait in this module is a [`Future`] built from
//! futures and wakers only: async code awaits it, and any executor polls it
//! with its own [`Waker`](core::task::Waker). Every primitive also has
//! `try_` forms that never wait. The core of this module makes no system
//! call: no futex, no thread park, no sleep and no clock read. The one
//! exception is inside the lock-free queues the primitives use, which recycle
//! waiter nodes and yield the thread when they meet contention. The other
//! exception is the
//! `blocking_*` methods, which exist for threads that may block and park
//! the calling thread until their wait is over. Called on a nuclei shard
//! (inside one of its tasks) they run a nested `block_on` instead, so the
//! shard keeps running its other tasks; called inside another runtime's task
//! they block the thread that task runs on.
//!
//! # Names and signatures
//!
//! [`Mutex`](crate::sync::Mutex), [`RwLock`](crate::sync::RwLock), [`Semaphore`](crate::sync::Semaphore), [`Notify`](crate::sync::Notify), [`Barrier`](crate::sync::Barrier),
//! [`OnceCell`](crate::sync::OnceCell) and [`SetOnce`](crate::sync::SetOnce) take the names, signatures, error types and
//! documented behavior of the established async `sync` API, so code
//! written against that API compiles against these. The
//! guards and errors sit at the same paths under `nuclei::sync`, and the
//! waits are named futures (`Lock`, `Read`, `Acquire`, and so on) rather
//! than opaque ones. A semaphore holds at most [`Semaphore::MAX_PERMITS`](crate::sync::Semaphore::MAX_PERMITS)
//! permits, which is `usize::MAX >> 7`. What this module adds is the
//! [`Event`](crate::sync::Event), the [`Latch`](crate::sync::Latch), the [`Lazy`](crate::sync::Lazy) value, the upgradable read of the
//! [`RwLock`](crate::sync::RwLock), the reentrant locks and `OnceCell::get_or_init_racy`.
//! The [`mpsc`](crate::sync::mpsc) channels follow the same rule for the established channel API,
//! with their own progress notes.
//! The [`local`](crate::sync::local) module adds single-shard twins of the
//! channels, the semaphore and the mutex, with no atomic instruction on any
//! path.
//!
//! # What every primitive guarantees
//!
//! - **No runtime.** A contended acquire queues a waiter, registers the
//!   caller's `Waker` and returns `Pending`. What a wake does belongs to
//!   the caller's executor. Within one poll, a contended acquire of a lock
//!   or a semaphore may first spin for a short while (up to the lock's
//!   spin budget, a count of pause instructions it adapts to how long holds
//!   last, about 5 microseconds at most on the cores it was tuned on):
//!   before it queues, and again after a retry it lost. The spin makes no
//!   system call, reads no clock, and ends in the same queueing or the same
//!   `Pending` as before. Locks whose holders keep them across long waits
//!   of their own do not spin, and their contended callers queue at once:
//!   the once-cell's initializer turn, for example.
//! - **Fast under contention.** The locks and the semaphore let a caller
//!   take what is free even while others wait, so a release never waits
//!   for a suspended task to run: the thread that released takes the lock
//!   straight back.
//! - **Bounded bypass.** A release wakes the oldest waiters it could
//!   serve to compete again. One that loses [`MAX_BYPASS`](crate::sync::MAX_BYPASS) times is owed a
//!   handoff: nobody gets in ahead of it, and the next release hands it
//!   what it waits for directly. Fairness is counted, never timed.
//! - **Cancellation-safe.** Dropping a pending future withdraws its
//!   waiter. If something was handed to it concurrently, the drop passes
//!   it on. No wakeup is lost. (A [`Barrier`](crate::sync::Barrier) wait is the one exception: an
//!   arrival that was counted stays counted.)
//! - **Cooperative on a nuclei shard.** In a task on a nuclei shard (or in
//!   `block_on`'s own future), a receive of [`mpsc`](crate::sync::mpsc),
//!   [`oneshot`](crate::sync::oneshot), [`broadcast`](crate::sync::broadcast)
//!   or [`watch`](crate::sync::watch) that completes spends one operation of
//!   the task's cooperative budget, as IO does
//!   ([`task::coop`](crate::task::coop)), and so does an acquire of a
//!   semaphore, a lock (the reentrant ones too) or a bounded send that
//!   completes at its first attempt. When the budget is spent the call
//!   completes nothing, wakes its task and returns `Pending`, so a loop on a
//!   channel or a lock that is always ready yields to the shard's other
//!   tasks. A wait that queued and was granted spends nothing: giving the
//!   grant back would cost it its place. The `try_` forms never spend, and
//!   off a nuclei shard there is no budget. The events, latches, barriers,
//!   once-cells and `Notify` keep no budget.
//! - **Reentrancy by [`Owner`](crate::sync::Owner).** The reentrant locks are keyed by an
//!   owner token rather than a thread, because a task can move between
//!   threads and many tasks can share one.
//! - **Every target.** State lives in word-sized atomics. On
//!   single-threaded wasm the same code runs on one thread, where only
//!   interleaved tasks contend. The fast paths allocate nothing; a waiter
//!   node comes from a per-primitive free list and reaches the allocator
//!   only when that list is empty.
//!
//! # How a wait works
//!
//! Each primitive keeps one state word, a lock-free stack of arriving
//! waiters and a FIFO of the waiters already sorted. Uncontended calls
//! touch the state word only, one atomic read-modify-write each. Anything
//! that can make a waiter runnable (a release, a new waiter, a
//! cancellation) takes the drain role with one compare-and-swap, or, when
//! another thread holds it, leaves a mark for the holder and returns, so
//! no call ever waits for another. The holder wakes what the state allows
//! (or, while a handoff is owed, grants it in queue order), rechecks until
//! nothing changed while it worked, and calls the wakers after it has
//! given the role up.
//!
//! # Progress
//!
//! Every operation is lock-free, not wait-free. No call waits for another
//! thread, and some call always completes; but a call may do work on
//! behalf of others, and how much depends on them.
//!
//! - Uncontended, an acquire and a release are one atomic
//!   read-modify-write each.
//! - A call that can make a waiter runnable (a release with waiters, a new
//!   waiter, a cancellation, a notification, a semaphore's `close`, a write
//!   guard's `downgrade`, `add_permits`) may serve the queue itself. It
//!   returns after one pass over the queue, plus at most one more pass for
//!   each of these that lands while it works: a waiter that queues, a
//!   waiter that withdraws, a waiter that claims the handoff it is owed, and
//!   a release that finds a node queued.
//! - `forget_permits` wakes nobody: it is a compare-and-swap loop on the
//!   state word, which retries only when another call changed the word
//!   first. A barrier's arrival is one `fetch_add`, followed, for the
//!   arrival that completes a group, by the pass that wakes the group.
//! - A release over an empty queue earns no pass; a release that finds a
//!   node queued earns one. So releases alone never keep a thread at work.
//!   Waiters that keep queueing or withdrawing can, one pass each: every
//!   such pass is another thread's own operation getting done, which is
//!   what lock-free means, and there is no cap on the passes of one call.
//!
//! Besides the primitives, this module re-exports a lock-free map
//! ([`HopscotchMap`](crate::sync::HopscotchMap)), queues
//! ([`SegQueue`](crate::sync::SegQueue), [`ArrayQueue`](crate::sync::ArrayQueue)) and
//! [`Atom`](crate::sync::Atom), so one module holds every synchronization structure.

/// Defines a constructor as a `const fn` in an ordinary build and as a
/// plain `fn` under `--cfg loom`, whose instrumented atomics cannot be
/// built in a constant context.
macro_rules! loom_const_fn {
    ($(#[$attr:meta])* $vis:vis fn $name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty $body:block) => {
        $(#[$attr])*
        #[cfg(not(all(loom, test)))]
        $vis const fn $name($($arg: $ty),*) -> $ret $body

        $(#[$attr])*
        #[cfg(all(loom, test))]
        $vis fn $name($($arg: $ty),*) -> $ret $body
    };
}

#[cfg(not(all(loom, test)))]
pub(crate) mod atomic_waker;
#[cfg(all(loom, test))]
#[doc(hidden)]
pub mod atomic_waker;
mod barrier;
pub mod broadcast;
mod cancellation;
mod contend;
mod event;
mod fair_semaphore;
pub(crate) mod internal;
mod latch;
mod lazy;
mod list;
pub mod local;
#[cfg(all(loom, test))]
#[doc(hidden)]
pub(crate) mod loom_model;
#[cfg(all(test, loom))]
mod loom_tests;
pub mod mpsc;
mod mutex;
mod notify;
mod once_cell;
pub mod oneshot;
// See the note on its re-export below.
#[allow(dead_code)]
mod owned_lock;
mod owner;
mod queue;
mod raw_rwlock;
mod raw_semaphore;
mod reentrant_mutex;
mod reentrant_rwlock;
mod rwlock;
mod semaphore;
mod set_once;
mod spin;
#[cfg(all(test, not(loom)))]
pub(crate) mod test_support;
mod waiter;
pub mod watch;

pub use barrier::{Barrier, BarrierWait, BarrierWaitResult};
pub use cancellation::{
    CancellationToken, DropGuard, DropGuardRef, WaitForCancellationFuture,
    WaitForCancellationFutureOwned,
};
pub use contend::MAX_BYPASS;
pub use event::{Event, EventWait};
pub use latch::{Latch, LatchWait};
pub use lazy::Lazy;
pub use mutex::{
    Lock, LockOwned, MappedMutexGuard, Mutex, MutexGuard, OwnedMappedMutexGuard, OwnedMutexGuard,
    TryLockError,
};
pub use notify::{Notified, Notify, OwnedNotified};
pub use once_cell::{OnceCell, SetError};
// vertexia: the owner-named lock has no consumer in this crate yet; its
// tests run in `owned_lock`. Trim it if it stays unused.
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use owned_lock::between_take_and_grant;
pub use owner::{Owner, ThreadOwner};
pub use reentrant_mutex::{ReentrantMutex, ReentrantMutexGuard, ReentrantMutexLock};
pub use reentrant_rwlock::{
    ReadHeldByOwner, ReentrantRead, ReentrantReadGuard, ReentrantRwLock, ReentrantWrite,
    ReentrantWriteGuard,
};
pub use rwlock::{
    OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, Read, ReadOwned,
    RwLock, RwLockMappedWriteGuard, RwLockReadGuard, RwLockUpgradableReadGuard, RwLockWriteGuard,
    UpgradableRead, Upgrade, Write, WriteOwned,
};
pub use semaphore::{
    Acquire, AcquireError, AcquireOwned, OwnedSemaphorePermit, Semaphore, SemaphorePermit,
    TryAcquireError,
};
pub use set_once::{SetOnce, SetOnceError};

/// Named future types, at the conventional `sync::futures` path.
pub mod futures {
    pub use super::notify::{Notified, OwnedNotified};
}

pub use kovan::Atom;
pub use kovan_map::HopscotchMap;
pub use kovan_queue::array_queue::ArrayQueue;
pub use kovan_queue::seg_queue::{SegQueue, TryPopError};

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn send_sync<T: Send + Sync>() {}

    fn future_is_send_sync<F: core::future::Future + Send + Sync>(_: &F) {}

    #[test]
    fn handles_futures_and_guards_are_send_and_sync() {
        send_sync::<Mutex<u8>>();
        send_sync::<Lock<'static, u8>>();
        send_sync::<LockOwned<u8>>();
        send_sync::<MutexGuard<'static, u8>>();
        send_sync::<OwnedMutexGuard<u8>>();
        send_sync::<MappedMutexGuard<'static, u8>>();
        send_sync::<OwnedMappedMutexGuard<u8, u8>>();
        send_sync::<RwLock<u8>>();
        send_sync::<Read<'static, u8>>();
        send_sync::<ReadOwned<u8>>();
        send_sync::<Write<'static, u8>>();
        send_sync::<RwLockReadGuard<'static, u8>>();
        send_sync::<RwLockWriteGuard<'static, u8>>();
        send_sync::<RwLockMappedWriteGuard<'static, u8>>();
        send_sync::<WriteOwned<u8>>();
        send_sync::<OwnedRwLockReadGuard<u8, u8>>();
        send_sync::<OwnedRwLockWriteGuard<u8>>();
        send_sync::<OwnedRwLockMappedWriteGuard<u8, u8>>();
        send_sync::<RwLockUpgradableReadGuard<'static, u8>>();
        send_sync::<UpgradableRead<'static, u8>>();
        send_sync::<Upgrade<'static, u8>>();
        send_sync::<ReadHeldByOwner>();
        send_sync::<Semaphore>();
        send_sync::<Acquire<'static>>();
        send_sync::<AcquireOwned>();
        send_sync::<SemaphorePermit<'static>>();
        send_sync::<OwnedSemaphorePermit>();
        send_sync::<Notify>();
        send_sync::<Notified<'static>>();
        send_sync::<OwnedNotified>();
        send_sync::<TryLockError>();
        send_sync::<AcquireError>();
        send_sync::<TryAcquireError>();
        send_sync::<SetError<u8>>();
        send_sync::<SetOnceError<u8>>();
        send_sync::<SetOnce<u8>>();
        send_sync::<Event>();
        send_sync::<EventWait<'static>>();
        send_sync::<Latch>();
        send_sync::<LatchWait<'static>>();
        send_sync::<Barrier>();
        send_sync::<BarrierWait<'static>>();
        send_sync::<OnceCell<u8>>();
        send_sync::<Lazy<u8>>();
        send_sync::<ReentrantMutex<core::cell::RefCell<u8>>>();
        send_sync::<ReentrantRwLock<u8>>();
        // An owner moves with its task but is never shared (see `Owner`).
        send::<Owner>();
        // A value that may be shared but not moved: the upgradable read
        // can still be shared, and its doc test pins that it cannot move.
        sync::<RwLockUpgradableReadGuard<'static, Option<std::sync::MutexGuard<'static, ()>>>>();
    }

    fn send<T: Send>() {}

    fn sync<T: Sync>() {}

    #[test]
    fn async_entry_points_return_send_and_sync_futures() {
        let cell = OnceCell::<u8>::new();
        future_is_send_sync(&cell.get_or_init(|| async { 1 }));
        future_is_send_sync(&cell.get_or_try_init(|| async { Ok::<u8, ()>(1) }));
        let once = SetOnce::<u8>::new();
        future_is_send_sync(&once.wait());
    }
}
