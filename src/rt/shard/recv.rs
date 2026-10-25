//! A shard's buffers for multishot receives (io_uring's provided buffers):
//! the kernel fills one as bytes arrive, so a receive that waits holds no
//! buffer of its own, and one receive stays armed for every arrival.
//!
//! The ring's thread owns the group and re-adds its buffers. A completion
//! hands a buffer out as a [`RingChunk`], which the source's reader may take
//! on any thread; dropped, the chunk puts its buffer on the group's return
//! queue, and the ring's thread takes the queue in before it next waits.
//! The buffers stay mapped while any chunk lives (see `BufMem`), so a chunk
//! that outlives its shard still reads memory that is there.
//!
//! The bytes of a receive's chunks not given back yet are counted against
//! the group's per-receive cap: past it the ring cancels the receive, and
//! its source asks for a new one once its reader took them, so a reader
//! that falls behind stops taking buffers. The cancel bounds a receive from
//! the batch of arrivals in which it passed the cap on, not within that
//! batch: the kernel fills buffers for a socket with bytes waiting until the
//! group or the completion queue runs out, and a receive that finds the
//! group dry ends, its source reading through the shard's pool instead.

use std::io;
use std::sync::Arc;

use kovan_queue::seg_queue::SegQueue;

use crate::syscore::linux::iouring::ops::NOT_AN_OP;
use crate::syscore::linux::iouring::ring::Ring;
use crate::syscore::linux::iouring::ring::buf_ring::{BufMem, BufRing};

/// The buffer group every multishot receive of a shard draws from.
const GROUP: u16 = 0;

/// The share of the group one receive may hold before the ring stops it.
const CAP_SHARE: usize = 8;

/// A shard's buffer group: the ring of buffers, who holds each one handed
/// out, and the queue they come back on.
pub(crate) struct RecvGroup {
    ring: BufRing,
    back: Arc<Returns>,
    /// For each buffer handed out: the receive it went to and its bytes,
    /// counted off that receive when it comes back.
    owners: Vec<(u64, u32)>,
    cap: usize,
}

// SAFETY: the group is used by its ring's thread only. A shard is built on
// one thread and moved to the thread that runs it before its first wait (as
// its ring is, see `RingPoller`); the descriptor ring is written by that
// thread alone, and what chunks touch on other threads (`Returns`) is
// `Sync`.
unsafe impl Send for RecvGroup {}

/// What chunks share with their group: the buffers, and the queue of
/// buffers given back.
pub(crate) struct Returns {
    mem: Arc<BufMem>,
    queue: SegQueue<u16>,
}

/// Bytes `start..end` of a buffer a receive's completion handed out, read
/// on any thread; its buffer goes back to the group when it drops.
pub(crate) struct RingChunk {
    back: Arc<Returns>,
    bid: u16,
    start: u32,
    end: u32,
}

impl RecvGroup {
    /// A group of `entries` buffers (a power of two up to 32768) of `size`
    /// bytes each, registered on `ring`, every buffer given to the kernel.
    pub(crate) fn new(ring: &Ring, entries: u16, size: u32) -> io::Result<Self> {
        let mut buffers = BufRing::new(ring, GROUP, entries, size)?;
        for bid in 0..entries {
            buffers.add(bid)?;
        }
        buffers.commit();
        let back = Arc::new(Returns {
            mem: Arc::clone(buffers.memory()),
            queue: SegQueue::new(),
        });
        let total = usize::from(entries) * size as usize;
        Ok(Self {
            ring: buffers,
            back,
            owners: vec![(NOT_AN_OP, 0); usize::from(entries)],
            cap: (total / CAP_SHARE).max(size as usize),
        })
    }

    /// The group id receives name.
    pub(crate) fn bgid(&self) -> u16 {
        self.ring.bgid()
    }

    /// How many buffers the group has: at most this many completions of
    /// its receives can be queued at once.
    pub(crate) fn entries(&self) -> u16 {
        self.ring.entries()
    }

    /// The bytes one receive may hold before the ring stops it.
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    /// The chunk of buffer `bid` that a completion of receive `user_data`
    /// filled with `len` bytes; `None` for an id or a length outside the
    /// group (the buffer then goes back at once).
    pub(crate) fn hand_out(&mut self, user_data: u64, bid: u16, len: u32) -> Option<RingChunk> {
        let owner = self.owners.get_mut(usize::from(bid))?;
        if len > self.ring.buf_size() {
            return None;
        }
        *owner = (user_data, len);
        Some(RingChunk {
            back: Arc::clone(&self.back),
            bid,
            start: 0,
            end: len,
        })
    }

    /// Gives buffer `bid` to the kernel again at once: a completion handed
    /// it out with nothing for anyone.
    pub(crate) fn give_back(&mut self, bid: u16) {
        if let Some(owner) = self.owners.get_mut(usize::from(bid)) {
            *owner = (NOT_AN_OP, 0);
        }
        if self.ring.add(bid).is_ok() {
            self.ring.commit();
        }
    }

    /// Takes in the buffers given back, gives them to the kernel again, and
    /// counts each off the receive it went to with `release(user_data,
    /// len)`.
    pub(crate) fn take_back(&mut self, mut release: impl FnMut(u64, u32)) {
        let mut any = false;
        while let Ok(bid) = self.back.queue.try_pop() {
            if let Some(owner) = self.owners.get_mut(usize::from(bid)) {
                let (user_data, len) = core::mem::replace(owner, (NOT_AN_OP, 0));
                if user_data != NOT_AN_OP {
                    release(user_data, len);
                }
            }
            // A full ring cannot happen: a buffer comes back once per
            // hand-out, so at most `entries` are added since the last commit.
            if self.ring.add(bid).is_ok() {
                any = true;
            }
        }
        if any {
            self.ring.commit();
        }
    }
}

#[cfg(test)]
impl RecvGroup {
    /// The bytes of the buffers handed out and not given back yet.
    pub(crate) fn out(&self) -> usize {
        self.owners
            .iter()
            .filter(|(user_data, _)| *user_data != NOT_AN_OP)
            .map(|(_, len)| *len as usize)
            .sum()
    }
}

impl RingChunk {
    /// The bytes not taken yet.
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: the buffer was handed out and goes back only when this
        // chunk drops, so the kernel does not write it while it lives.
        unsafe { self.back.mem.bytes(self.bid, self.start, self.end) }.unwrap_or_default()
    }

    /// Takes `n` bytes off the front.
    pub(crate) fn consume(&mut self, n: usize) {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        self.start = self.start.saturating_add(n).min(self.end);
    }

    pub(crate) fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

impl Drop for RingChunk {
    fn drop(&mut self) {
        self.back.queue.push(self.bid);
    }
}

impl core::fmt::Debug for RingChunk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RingChunk")
            .field("bid", &self.bid)
            .field("len", &self.len())
            .finish()
    }
}
