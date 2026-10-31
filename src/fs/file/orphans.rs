//! Positional writes (and the changes of a range, which are writes in this)
//! whose future was dropped while they were out. Each
//! still lands, and the file keeps it until it did: every later write at the
//! cursor, cursor read, positional call and sync of the handle waits for it,
//! so a write that came before them is never reordered with them.
//!
//! The dropped future pushes its call onto the file's list with one
//! compare-and-swap and leaves the file state's holder a deferred bit; the
//! holder takes the whole list into the state and drives the calls to their
//! end in `poll_writes`. The list is only ever taken whole (one swap), so a
//! push never meets a node that was taken and made again at the same
//! address: the compare-and-swap cannot be fooled.

use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Poll, Waker};
use std::io;

use super::work::Work;
use crate::buf::owned::OwnedBuf;

/// A positional write left by its task: from a buffer of the pool, or from
/// the task's own buffer, which goes with the call.
pub(super) enum Orphan {
    Pool(Work<io::Result<()>>),
    Lent(Work<(io::Result<()>, OwnedBuf)>),
}

impl From<Work<io::Result<()>>> for Orphan {
    fn from(work: Work<io::Result<()>>) -> Self {
        Orphan::Pool(work)
    }
}

impl From<Work<(io::Result<()>, OwnedBuf)>> for Orphan {
    fn from(work: Work<(io::Result<()>, OwnedBuf)>) -> Self {
        Orphan::Lent(work)
    }
}

impl Orphan {
    /// Ready once the write landed, with its error if it failed; else
    /// `waker` is woken when it does.
    pub(super) fn poll(&mut self, waker: &Waker) -> Poll<Option<io::Error>> {
        match self {
            Orphan::Pool(work) => work.poll(waker).map(Result::err),
            Orphan::Lent(work) => work.poll(waker).map(|(result, _)| result.err()),
        }
    }

    /// Lets the write run to its end with nobody to take it.
    pub(super) fn let_go(self) {
        match self {
            Orphan::Pool(work) => work.let_go(),
            Orphan::Lent(work) => work.let_go(),
        }
    }
}

struct Node {
    orphan: Orphan,
    next: *mut Node,
}

/// The positional writes left by their tasks and not yet taken by the
/// file state, the last first.
pub(super) struct Orphans {
    head: AtomicPtr<Node>,
}

// SAFETY: the nodes are owned by the list alone between the push that
// publishes them and the swap that takes them, and their calls are `Send`.
unsafe impl Send for Orphans {}
// SAFETY: as for `Send`: a node is reached only by the push that makes it
// and the one swap that takes it.
unsafe impl Sync for Orphans {}

impl Orphans {
    pub(super) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(super) fn push(&self, orphan: Orphan) {
        let node = Box::into_raw(Box::new(Node {
            orphan,
            next: ptr::null_mut(),
        }));
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: the node is this call's until the exchange publishes it.
            unsafe { (*node).next = head };
            // Release: the taker reads the node after it sees it in the list.
            match self
                .head
                .compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => head = now,
            }
        }
    }

    /// Moves every write pushed so far into `into`.
    pub(super) fn take(&self, into: &mut Vec<Orphan>) {
        if self.head.load(Ordering::Relaxed).is_null() {
            return;
        }
        // Acquire: pairs with the Release of `push`.
        let mut node = self.head.swap(ptr::null_mut(), Ordering::Acquire);
        while !node.is_null() {
            // SAFETY: taken off the list by the swap, so this call's alone;
            // made by `Box::into_raw` in `push`.
            let taken = unsafe { Box::from_raw(node) };
            node = taken.next;
            into.push(taken.orphan);
        }
    }
}

impl Drop for Orphans {
    /// The file state is gone: the writes still land, with nobody waiting.
    fn drop(&mut self) {
        let mut left = Vec::new();
        self.take(&mut left);
        for orphan in left {
            orphan.let_go();
        }
    }
}
