//! The FIFO of waiters the single-shard primitives share.
//!
//! A slab of entries linked into one list by index, as the timer wheel's
//! slab is: pushing at the back, unlinking any entry (a cancelled wait) and
//! reading the front are O(1), and the slab grows to the most waiters alive
//! at once and no further, since a freed entry goes to a free list and is
//! reused. A [`Key`] carries the entry's generation, so a key whose entry is
//! gone never reaches the entry that reuses its place.
//!
//! An entry is `Waiting` while it is linked. Granting or closing the front
//! entry unlinks it and takes its waker, but leaves the entry in the slab
//! until its owner takes the result (or is dropped): nothing is lazily
//! tombstoned in the list.
//!
//! Every method is one non-reentrant step on the slab and runs no user code:
//! a waker is cloned by the caller before a step and dropped or woken by the
//! caller after it, which is why the steps hand wakers back instead of
//! dropping them.

#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::task::Waker;

/// The end of the list, and of the free list.
const NIL: usize = usize::MAX;

/// A waiter in the list. Stale once its entry is freed: every call with it
/// then does nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    index: usize,
    generation: u32,
}

/// Where an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Linked, waiting for its turn.
    Waiting,
    /// Served: unlinked, and its owner has not taken the result yet.
    Granted,
    /// Ended without being served: unlinked, and its owner has not taken the
    /// result yet.
    Closed,
    /// On the free list, or a stale key.
    Free,
}

struct Entry {
    waker: Option<Waker>,
    need: usize,
    state: State,
    prev: usize,
    next: usize,
    generation: u32,
}

struct Slab {
    entries: Vec<Entry>,
    free: usize,
    head: usize,
    tail: usize,
}

impl Slab {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: NIL,
            head: NIL,
            tail: NIL,
        }
    }

    /// The live entry of `key`, if it is still the one `key` was made for.
    fn live(&mut self, key: Key) -> Option<&mut Entry> {
        self.entries
            .get_mut(key.index)
            .filter(|e| e.generation == key.generation && e.state != State::Free)
    }

    fn push(&mut self, need: usize, waker: Waker) -> Key {
        let tail = self.tail;
        let entry = Entry {
            waker: Some(waker),
            need,
            state: State::Waiting,
            prev: tail,
            next: NIL,
            generation: 0,
        };
        let index = if self.free == NIL {
            self.entries.push(entry);
            self.entries.len() - 1
        } else {
            let index = self.free;
            let slot = &mut self.entries[index];
            self.free = slot.next;
            let generation = slot.generation;
            *slot = Entry {
                generation,
                ..entry
            };
            index
        };
        if tail == NIL {
            self.head = index;
        } else {
            self.entries[tail].next = index;
        }
        self.tail = index;
        Key {
            index,
            generation: self.entries[index].generation,
        }
    }

    /// Takes the entry at `index` out of the list.
    fn unlink(&mut self, index: usize) {
        let (prev, next) = (self.entries[index].prev, self.entries[index].next);
        if prev == NIL {
            self.head = next;
        } else {
            self.entries[prev].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.entries[next].prev = prev;
        }
    }

    /// Frees the entry at `index`, which is not linked; its key goes stale.
    fn free_entry(&mut self, index: usize) {
        let free = self.free;
        let slot = &mut self.entries[index];
        slot.state = State::Free;
        slot.waker = None;
        slot.generation = slot.generation.wrapping_add(1);
        slot.next = free;
        self.free = index;
    }

    /// Unlinks the front entry, marks it `state` and takes its waker.
    fn pop_front(&mut self, state: State) -> Option<(usize, Option<Waker>)> {
        let index = self.head;
        if index == NIL {
            return None;
        }
        self.unlink(index);
        let entry = &mut self.entries[index];
        entry.state = state;
        Some((entry.need, entry.waker.take()))
    }
}

/// The shared FIFO. `!Send` and `!Sync`: it belongs to one shard.
pub(crate) struct Waiters {
    slab: UnsafeCell<Slab>,
    _shard: PhantomData<*mut ()>,
}

impl Waiters {
    pub(crate) const fn new() -> Self {
        Self {
            slab: UnsafeCell::new(Slab::new()),
            _shard: PhantomData,
        }
    }

    /// Runs one step on the slab.
    #[inline]
    fn step<R>(&self, f: impl FnOnce(&mut Slab) -> R) -> R {
        // SAFETY: the list is `!Sync`, so only this thread reaches it, and
        // every step is a plain slab operation that runs no user code and
        // calls no other step, so this is the only live borrow.
        f(unsafe { &mut *self.slab.get() })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.step(|s| s.head == NIL)
    }

    /// Queues a waiter that needs `need`, at the back.
    pub(crate) fn push(&self, need: usize, waker: Waker) -> Key {
        self.step(|s| s.push(need, waker))
    }

    /// Grants the front waiter when its need is at most `available`: answers
    /// its need and its waker, to wake outside the step.
    pub(crate) fn grant_front_if(&self, available: usize) -> Option<(usize, Option<Waker>)> {
        self.step(|s| {
            if s.head != NIL && s.entries[s.head].need <= available {
                s.pop_front(State::Granted)
            } else {
                None
            }
        })
    }

    /// Ends the front waiter without serving it: answers its waker.
    pub(crate) fn close_front(&self) -> Option<Option<Waker>> {
        self.step(|s| s.pop_front(State::Closed).map(|(_, waker)| waker))
    }

    /// The state of `key`, after making `waker` the one a waiting entry
    /// wakes.
    pub(crate) fn refresh(&self, key: Key, waker: &Waker) -> State {
        let (state, same) = self.step(|s| {
            s.live(key).map_or((State::Free, true), |e| {
                (
                    e.state,
                    e.state != State::Waiting
                        || e.waker.as_ref().is_some_and(|w| w.will_wake(waker)),
                )
            })
        });
        if !same {
            let fresh = waker.clone();
            let old = self.step(|s| match s.live(key) {
                Some(e) if e.state == State::Waiting => e.waker.replace(fresh),
                _ => Some(fresh),
            });
            drop(old);
        }
        state
    }

    /// Frees a granted or closed entry once its owner took the result.
    pub(crate) fn take(&self, key: Key) {
        self.step(|s| {
            if s.live(key).is_some_and(|e| e.state != State::Waiting) {
                s.free_entry(key.index);
            }
        });
    }

    /// Removes the entry of `key` whatever its state. Answers the state it
    /// was in, whether it was the front of the list, and its waker to drop
    /// outside the step.
    pub(crate) fn cancel(&self, key: Key) -> (State, bool, Option<Waker>) {
        self.step(|s| {
            let Some(entry) = s.live(key) else {
                return (State::Free, false, None);
            };
            let state = entry.state;
            let waker = entry.waker.take();
            let was_front = state == State::Waiting && s.head == key.index;
            if state == State::Waiting {
                s.unlink(key.index);
            }
            s.free_entry(key.index);
            (state, was_front, waker)
        })
    }
}
