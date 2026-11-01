//! A datagram socket's calls made straight on its descriptor, with no guard
//! taken, while its side holds nothing (proofs/tla/dgram).
//!
//! A task receives and sends its own datagrams (`receive_now`, `send_now`),
//! and a side that holds nothing, waits for no edge and has no operation out
//! adds nothing to that call: the call may go to the kernel as the poll
//! begins. Each side's last holder leaves a word as it lets go: `HOLDS` when
//! the call must go through the side, and, on the read side, the side's
//! count of edges. A call that finds `HOLDS` clear calls the kernel at once.
//! A receive the kernel answers "not now" takes the side with the word it
//! saw: if no edge was counted since, the socket was empty after the last
//! edge, and the call waits for the next one without asking the kernel
//! again; if one was, it asks again under the guard.
//!
//! A send's "not now" goes through the side, which raises `HOLDS` before it
//! asks the kernel again: the driver lets a writable edge go by while
//! `HOLDS` is clear (nobody waits for room then), and a raised one is seen
//! by every edge that follows the call that found the socket full.

use core::sync::atomic::{AtomicU32, Ordering};

use super::Kind;

/// The call goes through the side.
pub(super) const HOLDS: u32 = 1;

/// What each side's last holder left for the calls made with no guard.
pub(super) struct Quick {
    /// `HOLDS`, and above it the read side's count of edges.
    read: AtomicU32,
    /// `HOLDS`.
    write: AtomicU32,
    /// The source is a datagram socket's: its holders leave the words.
    on: bool,
}

impl Quick {
    /// The words of a source of `kind`. A datagram socket's first receive
    /// goes through the side, which settles how it receives; any other
    /// source's write word stays raised, so every writable edge reaches its
    /// side.
    pub(super) fn new(kind: Kind) -> Self {
        Self {
            read: AtomicU32::new(HOLDS),
            write: AtomicU32::new(if kind.datagrams() { 0 } else { HOLDS }),
            on: kind.datagrams(),
        }
    }

    /// Whether the source's holders leave the words.
    #[inline(always)]
    pub(super) fn on(&self) -> bool {
        self.on
    }

    /// The read side's word, if a receive may go to the kernel at once.
    #[inline(always)]
    pub(super) fn read_clear(&self) -> Option<u32> {
        let word = self.read.load(Ordering::Acquire);
        (word & HOLDS == 0).then_some(word)
    }

    /// Whether a send may go to the kernel at once.
    #[inline(always)]
    pub(super) fn write_clear(&self) -> bool {
        self.write.load(Ordering::Acquire) & HOLDS == 0
    }

    /// The read side's holder leaves its word ([`read_word`]).
    #[inline(always)]
    pub(super) fn leave_read(&self, word: u32) {
        self.read.store(word, Ordering::Release);
    }

    /// The write side's holder leaves whether the next send goes through it.
    #[inline(always)]
    pub(super) fn leave_write(&self, holds: bool) {
        self.write.store(u32::from(holds), Ordering::Release);
    }

    /// The write side's holder is about to ask the kernel to take a datagram:
    /// from now on every writable edge reaches the side. Sequentially
    /// consistent, so the raise is seen before the call is made.
    #[inline(always)]
    pub(super) fn hold_write(&self) {
        if self.write_clear() {
            self.write.store(HOLDS, Ordering::SeqCst);
        }
    }
}

/// The read side's word for `edges` edges counted, and `holds`.
#[inline(always)]
pub(super) fn read_word(edges: u32, holds: bool) -> u32 {
    (edges << 1) | u32::from(holds)
}
