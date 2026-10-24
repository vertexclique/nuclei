//! Owner tokens: who holds a reentrant lock.
//!
//! A reentrant lock lets its holder lock it again. "Its holder" cannot be
//! the current thread, because a task can move between threads and many
//! tasks can share one; so the reentrant locks take an [`Owner`] instead.
//!
//! Soundness rests on one rule: every guard an owner holds lives on one
//! thread at a time. A reentrant guard hands out `&T`, and two threads
//! holding `&T` to a `T` that is not `Sync` (a `RefCell`, say) would race.
//! So an `Owner` can move between threads but cannot be shared across
//! them or cloned, and every reentrant guard and future borrows the
//! `Owner` it was taken with, which pins it to that owner's thread for as
//! long as it lives.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::num::NonZeroU64;
use core::ops::Deref;
use core::sync::atomic::{AtomicU64, Ordering};

/// Ids are never reused, so a stale id can never name a live owner.
///
/// Plain `core` atomics, not the loom-aware ones of `internal`: a `static`
/// needs a `const` constructor, which loom's atomics lack, and the counter is
/// not state a model checks.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

std::thread_local! {
    static THREAD_ID: Cell<u64> = const { Cell::new(0) };
}

fn fresh_id() -> NonZeroU64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // Counting from one, 2^64 ids cannot run out.
    NonZeroU64::new(id).unwrap_or(NonZeroU64::MIN)
}

/// A token that identifies the holder of a reentrant lock.
///
/// [`Owner::new`] makes a token for a task or any caller-defined scope;
/// [`Owner::thread`] gives the calling thread's own token, for code that
/// runs on a fixed thread. A token can move to another thread with its task,
/// but it is neither `Clone` nor `Sync`, so it is only ever in one place.
///
/// ```
/// use nuclei::sync::{Owner, ReentrantMutex};
/// use std::cell::RefCell;
///
/// let lock = ReentrantMutex::new(RefCell::new(Vec::new()));
/// let me = Owner::new();
/// let outer = lock.try_lock(&me).expect("free");
/// let inner = lock.try_lock(&me).expect("the same owner re-enters");
/// inner.borrow_mut().push(1);
/// drop(inner);
/// assert!(lock.try_lock(&Owner::new()).is_none(), "another owner waits");
/// drop(outer);
/// ```
pub struct Owner {
    id: NonZeroU64,
    _not_sync: PhantomData<Cell<()>>,
}

impl Owner {
    /// A token no other owner has ever had.
    pub fn new() -> Self {
        Self {
            id: fresh_id(),
            _not_sync: PhantomData,
        }
    }

    /// The calling thread's token: the same owner for every call on this
    /// thread, for as long as it lives.
    pub fn thread() -> ThreadOwner {
        let id = THREAD_ID.with(|slot| match NonZeroU64::new(slot.get()) {
            Some(id) => id,
            None => {
                let id = fresh_id();
                slot.set(id.get());
                id
            }
        });
        ThreadOwner {
            owner: Self {
                id,
                _not_sync: PhantomData,
            },
            _not_send: PhantomData,
        }
    }

    pub(super) fn id(&self) -> u64 {
        self.id.get()
    }
}

impl Default for Owner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Owner").field(&self.id).finish()
    }
}

/// The calling thread's [`Owner`], from [`Owner::thread`].
///
/// It cannot leave its thread, since another call on the thread would
/// produce the same owner.
pub struct ThreadOwner {
    owner: Owner,
    _not_send: PhantomData<*const ()>,
}

impl Deref for ThreadOwner {
    type Target = Owner;

    fn deref(&self) -> &Owner {
        &self.owner
    }
}

impl fmt::Debug for ThreadOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ThreadOwner").field(&self.owner.id).finish()
    }
}
