//! The queued requests of one thread's owners, so an owner's entry wakes the
//! other requests the same owner has queued on the lock.
//!
//! A reentrant request looks for its owner first on every poll, but a queued
//! one is polled again only when woken, and nothing in the lock wakes it when
//! its owner enters through another request. So a request that answers
//! `Pending` while queued registers here (lock, owner, its waker), and an
//! entry that stores the owner's id wakes the registered requests of that
//! lock and owner, which then find the owner and complete.
//!
//! Every request borrows its `Owner`, which is not `Sync`, so it stays on one
//! thread and a thread-local registry serves. Invariants: an entry exists
//! exactly while its [`Ticket`] does and has been registered; the registry is
//! never borrowed while a waker runs; a registry no request uses costs an
//! entry one load.

#[cfg(all(test, not(loom)))]
pub(super) use real::queued_here;
#[cfg(not(all(loom, test)))]
pub(super) use real::{Ticket, wake_siblings};
#[cfg(all(loom, test))]
pub(super) use stub::{Ticket, wake_siblings};

#[cfg(not(all(loom, test)))]
mod real {
    use core::cell::{Cell, RefCell};
    use core::task::Waker;

    use crate::sync::internal::during_unwind;

    std::thread_local! {
        /// How many entries the registry holds.
        static COUNT: Cell<usize> = const { Cell::new(0) };
        /// The next ticket number.
        static NEXT: Cell<u64> = const { Cell::new(1) };
        static QUEUED: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    }

    struct Entry {
        ticket: u64,
        lock: usize,
        owner: u64,
        /// Taken when the entry's request is woken; set again by its next
        /// registration.
        waker: Option<Waker>,
    }

    /// A request's place in the registry: zero until it registers, and it
    /// leaves when dropped.
    pub(in crate::sync) struct Ticket(Cell<u64>);

    impl Ticket {
        pub(in crate::sync) const fn new() -> Self {
            Self(Cell::new(0))
        }

        /// The ticket's number, for an entry that wakes everyone else.
        pub(in crate::sync) fn number(&self) -> u64 {
            self.0.get()
        }

        /// The request answered `Pending` while queued on `lock` for `owner`:
        /// it is to be woken when `owner` enters through another request.
        pub(in crate::sync) fn register(&self, lock: usize, owner: u64, waker: &Waker) {
            let stale = QUEUED.with_borrow_mut(|queued| {
                let ticket = self.0.get();
                if ticket == 0 {
                    let fresh = NEXT.get();
                    NEXT.set(fresh + 1);
                    queued.push(Entry {
                        ticket: fresh,
                        lock,
                        owner,
                        waker: Some(waker.clone()),
                    });
                    self.0.set(fresh);
                    COUNT.set(COUNT.get() + 1);
                    return None;
                }
                let entry = queued.iter_mut().find(|entry| entry.ticket == ticket)?;
                match &entry.waker {
                    Some(held) if held.will_wake(waker) => None,
                    _ => entry.waker.replace(waker.clone()),
                }
            });
            // A waker's drop is user code: outside the borrow.
            drop(stale);
        }

        /// The request left the queue or ended.
        pub(in crate::sync) fn unregister(&self) {
            let ticket = self.0.replace(0);
            if ticket == 0 {
                return;
            }
            let gone = QUEUED
                .try_with(|queued| {
                    let mut queued = queued.borrow_mut();
                    let at = queued.iter().position(|entry| entry.ticket == ticket)?;
                    COUNT.set(COUNT.get() - 1);
                    Some(queued.swap_remove(at))
                })
                .ok()
                .flatten();
            drop(gone);
        }
    }

    impl Drop for Ticket {
        fn drop(&mut self) {
            self.unregister();
        }
    }

    /// How many requests this thread has registered.
    #[cfg(test)]
    pub(in crate::sync) fn queued_here() -> usize {
        COUNT.get()
    }

    /// The registered waker of another request on `lock` for `owner`, taken
    /// out of its entry.
    fn take_one(lock: usize, owner: u64, except: u64) -> Option<Waker> {
        QUEUED.with_borrow_mut(|queued| {
            queued
                .iter_mut()
                .find(|entry| {
                    entry.lock == lock
                        && entry.owner == owner
                        && entry.ticket != except
                        && entry.waker.is_some()
                })
                .and_then(|entry| entry.waker.take())
        })
    }

    /// `owner` entered `lock` (through the request numbered `except`, or
    /// none): wakes its other queued requests on it. One load when this
    /// thread has none queued.
    #[inline(always)]
    pub(in crate::sync) fn wake_siblings(lock: usize, owner: u64, except: u64) {
        if COUNT.get() != 0 {
            wake_registered(lock, owner, except);
        }
    }

    #[cold]
    #[inline(never)]
    fn wake_registered(lock: usize, owner: u64, except: u64) {
        /// A waker that panics must not strand the requests behind it.
        struct Rest(usize, u64, u64);
        impl Drop for Rest {
            fn drop(&mut self) {
                while let Some(waker) = take_one(self.0, self.1, self.2) {
                    during_unwind(|| waker.wake());
                }
            }
        }
        while let Some(waker) = take_one(lock, owner, except) {
            let rest = Rest(lock, owner, except);
            waker.wake();
            core::mem::forget(rest);
        }
    }
}

/// Under loom the registry is off: its threads share one OS thread, which a
/// thread-local cannot tell apart.
#[cfg(all(loom, test))]
mod stub {
    use core::task::Waker;

    pub(in crate::sync) struct Ticket;

    impl Ticket {
        pub(in crate::sync) const fn new() -> Self {
            Self
        }

        pub(in crate::sync) fn number(&self) -> u64 {
            0
        }

        pub(in crate::sync) fn register(&self, _lock: usize, _owner: u64, _waker: &Waker) {}

        pub(in crate::sync) fn unregister(&self) {}
    }

    pub(in crate::sync) fn wake_siblings(_lock: usize, _owner: u64, _except: u64) {}
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, hooked};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn a_waker_that_panics_leaves_the_registry_free_and_the_rest_woken() {
        let boom = || -> Box<dyn FnOnce()> { Box::new(|| panic!("a waker panicked")) };
        let counter = Polled::new(core::future::pending::<()>());
        let tickets = [Ticket::new(), Ticket::new(), Ticket::new(), Ticket::new()];
        tickets[0].register(1, 7, &hooked(None, Some(boom())));
        tickets[1].register(1, 7, &hooked(None, Some(boom())));
        tickets[2].register(1, 7, &counter.waker());
        // Another lock's and another owner's requests are left alone.
        tickets[3].register(2, 7, &counter.waker());
        let caught = catch_unwind(AssertUnwindSafe(|| wake_siblings(1, 7, 0)));
        assert!(caught.is_err(), "the first panic reaches the caller");
        assert_eq!(counter.wakes(), 1, "the request behind both was woken");
        // The registry is usable: a new registration and another wake.
        tickets[2].register(1, 7, &counter.waker());
        wake_siblings(1, 7, 0);
        assert_eq!(counter.wakes(), 2);
        drop(tickets);
        assert_eq!(queued_here(), 0);
    }
}
