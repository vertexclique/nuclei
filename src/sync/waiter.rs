//! Waiter nodes, the free list that recycles them, and the wake list a
//! drain pass hands woken waiters to.
//!
//! A contended future parks one [`Waiter`] in its primitive's queue. Up to
//! three parties hold the node: the future, until it completes or is
//! dropped; the queue, until a drain pass grants or prunes it; and a wake
//! list, while a waker taken from the node waits to be called. Each holds
//! one reference bit in the node's state word, and whichever party clears
//! the last bit returns the node to its primitive's [`Pool`]. No other path
//! frees a node, so a node is never reachable by a party that has let it
//! go, and no epoch scheme is needed.
//!
//! # State word
//!
//! | bits | meaning |
//! |---|---|
//! | 0-1 | phase: waiting, granted or cancelled; it leaves waiting once |
//! | 2 | registering: the future is writing a waker into a slot |
//! | 3 | armed: the current slot holds a waker nobody has taken |
//! | 4 | taking: a wake list is taking the waker from the taken slot |
//! | 5 | the current slot: where the future last registered |
//! | 6 | the taken slot: where the wake list takes from |
//! | 7 | nudged: a release asked the waiter to try again |
//! | 8 | owed: the waiter reached the bypass bound and is counted as owed |
//! | 9-11 | references: queue, future, wake list |
//!
//! A drainer *grants* a waiter (hands it what it waits for) or *nudges* it
//! (wakes it to compete again, leaving it in the queue). Either may claim
//! the armed waker: it marks the current slot as the taken one and hands
//! the node to a wake list, which takes the waker and calls it once the
//! drain role is given up.
//!
//! The waker lives in one of two slots so that neither side ever waits for
//! the other. The future writes a slot only while it holds the registering
//! bit, and it writes the slot that is not being taken; the wake list
//! reads only the taken slot, and only while it holds the taking bit. A
//! cancelling future reads the current slot only while it is armed. So no
//! two parties touch one slot at once. A grant or nudge that finds the
//! future registering, or a take already under way, only sets its bit: the
//! future reads that bit as it finishes registering, and the wake list,
//! once its take is done, claims a waker armed meanwhile whose node was
//! granted or nudged, and calls that one too.

mod node;
mod pool;
#[cfg(loom)]
pub(super) mod spin_probe;
mod wait;
mod wake;

pub(super) use node::{Waiter, release_queued};
pub(super) use pool::Pool;
pub(super) use wait::{Cancel, Park, Wait};
pub(super) use wake::WakeList;
