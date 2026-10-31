//! Synchronization primitives for the tasks of one shard.
//!
//! Everything here is for tasks that share one shard ([`task::spawn_local`](crate::task::spawn_local),
//! or the future `block_on` runs): the state sits in `Rc`s and plain cells,
//! so the types are `!Send` and `!Sync`, and no path executes an atomic
//! instruction or a fence. That makes them cheaper than their atomic
//! twins in [`sync`](crate::sync), which the op-count rows `local_mpsc` and
//! `local_oneshot` measure. The names and signatures mirror the atomic ones, so
//! code switches by changing the path: [`mpsc`] for
//! [`sync::mpsc`](crate::sync::mpsc), [`oneshot`] for
//! [`sync::oneshot`](crate::sync::oneshot), [`Semaphore`] for
//! [`sync::Semaphore`](crate::sync::Semaphore) and [`Mutex`] for
//! [`sync::Mutex`](crate::sync::Mutex). The errors are the atomic ones.
//!
//! ```compile_fail
//! fn send<T: Send>(_: T) {}
//! send(nuclei::sync::local::Mutex::new(0u8));
//! ```
//!
//! # Fairness
//!
//! Sends on a full bounded channel, acquires of the semaphore and locks of
//! the mutex are served strictly in arrival order, with no barging: permits
//! that come free are kept for the front waiter until its whole request
//! fits, and `try_` forms fail while anyone waits.
//!
//! # Cancel safety
//!
//! Dropping any future of this module at any await loses no value, no
//! permit and no wakeup, and leaks nothing: a waiting future gives up its
//! place in line (the next waiter is served if it was the front), and one
//! that was already granted its permits or the lock passes them on.
//!
//! # Cooperative scheduling
//!
//! The budget of [`task::coop`](crate::task::coop) applies as for the atomic
//! twins: a receive, an acquire or a lock that completes spends one
//! operation of the task's budget, and a bounded send or reservation that
//! completes at its first attempt does too. A wait that queued and was
//! granted spends nothing. The `try_` forms never spend.
//!
//! # Interior mutability
//!
//! The module keeps one rule. A cell is borrowed only inside one
//! non-reentrant step, and nothing that can run user code (a
//! [`Waker::wake`](core::task::Waker::wake), a value's `Drop`, a waker's
//! clone) runs while any of them is borrowed: the waker or the values are
//! taken out first, the borrow ends, and then they are woken or dropped.
//! After a wake the state is read again; no borrow is reused.

pub mod mpsc;
mod mutex;
pub mod oneshot;
mod semaphore;
mod waiters;

pub use mutex::{Lock, Mutex, MutexGuard};
pub use semaphore::{Acquire, Semaphore, SemaphorePermit};
