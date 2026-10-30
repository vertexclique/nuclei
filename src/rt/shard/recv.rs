//! A shard's buffers for multishot receives (io_uring's provided buffers):
//! the kernel fills one as bytes arrive, so a receive that waits holds no
//! buffer of its own, and one receive stays armed for every arrival.
//!
//! The ring's thread owns the group and re-adds its buffers. A completion
//! hands a buffer out as a [`RingChunk`], which the source's reader may take
//! on any thread; dropped, the chunk puts its buffer on the group's return
//! queue, and the ring's thread takes the queue in before it next waits. A
//! chunk dropped on the ring's own thread skips the queue: it goes onto the
//! group's local list, taken in with it.
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

/// The least a chunk of an incremental buffer counts against its receive's cap, so a peer that
/// sends a byte at a time is stopped after a bounded number of chunks, as one that fills buffers is.
const MIN_CHARGE: u32 = 256;

#[cfg(test)]
thread_local! {
    /// Whether a group made on this thread asks for incremental buffers: `None` asks and takes
    /// what the kernel gives, `Some(false)` never asks, for a test.
    pub(crate) static INCREMENTAL_FOR_TEST: core::cell::Cell<Option<bool>> =
        const { core::cell::Cell::new(None) };
}

/// A shard's buffer group: the ring of buffers, who holds each one handed
/// out, and the queue they come back on.
pub(crate) struct RecvGroup {
    ring: BufRing,
    back: Arc<Returns>,
    /// For each buffer: where the next piece of it starts, how many chunks of it are out, and
    /// whether the kernel is done filling it. Whole buffers (the plain group) are done at their
    /// one hand-out.
    fills: Vec<Fill>,
    /// Chunks given back on the ring's own thread, taken in with the rest.
    local: Vec<Back>,
    cap: usize,
    /// The buffers are filled in pieces (`IOU_PBUF_RING_INC`).
    incremental: bool,
    /// Bytes handed out in chunks not given back yet.
    #[cfg(test)]
    out: usize,
    /// Buffers given to the kernel again since the group was made.
    #[cfg(test)]
    readded: usize,
}

/// One buffer of the group.
#[derive(Clone, Copy, Default)]
struct Fill {
    used: u32,
    live: u32,
    done: bool,
}

/// What a chunk puts on the return queue: its buffer, the receive it counted against and its bytes.
#[derive(Clone, Copy)]
pub(crate) struct Back {
    bid: u16,
    owner: u64,
    len: u32,
}

// SAFETY: the group is used by its ring's thread only. A shard is built on
// one thread and moved to the thread that runs it before its first wait (as
// its ring is, see `RingPoller`); the descriptor ring is written by that
// thread alone, and what chunks touch on other threads (`Returns`) is
// `Sync`.
unsafe impl Send for RecvGroup {}

/// What chunks share with their group: the buffers, and the queue of
/// chunks given back. A buffer filled in pieces has many chunks out, so the
/// queue grows with them (a segment at a time), bounded by the bytes of the
/// group, each chunk holding at least one of them.
// vertexia: the queue is unbounded in chunks; a group-wide cap on live chunks that copies past it
// is the upgrade path if a peer sending single bytes across many connections shows up in a profile.
pub(crate) struct Returns {
    mem: Arc<BufMem>,
    queue: SegQueue<Back>,
}

/// Bytes `start..end` of a buffer a receive's completion handed out, read
/// on any thread; its buffer goes back to the group when the last of its
/// chunks drops and the kernel is done with it.
pub(crate) struct RingChunk {
    back: Arc<Returns>,
    bid: u16,
    start: u32,
    end: u32,
    owner: u64,
    len: u32,
}

/// The error of a group of `buffers` of `size` bytes the system had no memory for.
pub(crate) fn no_memory(buffers: u16, size: u32, err: &io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        format!(
            "no memory for the io_uring receive buffers, {buffers} of {size} bytes ({err}): lower recv_buffers or recv_buffer_size"
        ),
    )
}

impl RecvGroup {
    /// A group of `entries` buffers (a power of two up to 32768) of `size`
    /// bytes each, registered on `ring`, every buffer given to the kernel.
    /// The kernel fills them in pieces where it can (Linux 6.12), else
    /// whole.
    pub(crate) fn new(ring: &Ring, entries: u16, size: u32) -> io::Result<Self> {
        #[cfg(test)]
        let ask = INCREMENTAL_FOR_TEST.with(core::cell::Cell::get) != Some(false);
        #[cfg(not(test))]
        let ask = true;
        let (mut buffers, incremental) =
            match ask.then(|| BufRing::incremental(ring, GROUP, entries, size)) {
                Some(Ok(buffers)) => (buffers, true),
                // Refused (an older kernel) or not asked: the plain group.
                _ => (BufRing::new(ring, GROUP, entries, size)?, false),
            };
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
            fills: vec![Fill::default(); usize::from(entries)],
            local: Vec::with_capacity(usize::from(entries)),
            cap: (total / CAP_SHARE).max(size as usize),
            incremental,
            #[cfg(test)]
            out: 0,
            #[cfg(test)]
            readded: 0,
        })
    }

    /// Whether the buffers are filled in pieces.
    pub(crate) fn incremental(&self) -> bool {
        self.incremental
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

    /// The size of each buffer.
    pub(crate) fn buf_size(&self) -> u32 {
        self.ring.buf_size()
    }

    /// What `len` bytes of a chunk count against the cap of the receive that brought them.
    pub(crate) fn charge(&self, len: u32) -> usize {
        (if self.incremental {
            len.max(MIN_CHARGE)
        } else {
            len
        }) as usize
    }

    /// The chunk of buffer `bid` that a completion of receive `user_data`
    /// filled with `len` bytes (`more`: the buffer has room left, and the
    /// next completion on it continues after these); `None` for an id or a
    /// length outside the group (the buffer then goes back with
    /// [`give_back`](Self::give_back)).
    pub(crate) fn hand_out(
        &mut self,
        user_data: u64,
        bid: u16,
        len: u32,
        more: bool,
    ) -> Option<RingChunk> {
        let size = self.ring.buf_size();
        let fill = self.fills.get_mut(usize::from(bid))?;
        let start = if self.incremental { fill.used } else { 0 };
        let end = start.checked_add(len).filter(|&end| end <= size)?;
        fill.used = end;
        fill.live += 1;
        fill.done = !more;
        #[cfg(test)]
        {
            self.out += len as usize;
        }
        Some(RingChunk {
            back: Arc::clone(&self.back),
            bid,
            start,
            end,
            owner: user_data,
            len,
        })
    }

    /// Gives buffer `bid` back at once for a completion that handed `len` bytes of it to nobody
    /// (`more` as for [`hand_out`](Self::hand_out)).
    pub(crate) fn give_back(&mut self, bid: u16, len: u32, more: bool) {
        let size = self.ring.buf_size();
        let Some(fill) = self.fills.get_mut(usize::from(bid)) else {
            return;
        };
        fill.used = fill.used.saturating_add(len).min(size);
        fill.done = !more;
        if self.settle(bid) {
            self.ring.commit();
        }
    }

    /// Gives buffer `bid` to the kernel again if it is done and no chunk of it is out. Returns
    /// whether it did, for the caller to commit.
    fn settle(&mut self, bid: u16) -> bool {
        let Some(fill) = self.fills.get_mut(usize::from(bid)) else {
            return false;
        };
        if fill.live > 0 || !fill.done {
            return false;
        }
        *fill = Fill::default();
        #[cfg(test)]
        {
            self.readded += 1;
        }
        // A full ring cannot happen: a buffer comes back once per
        // fill, so at most `entries` are added since the last commit.
        self.ring.add(bid).is_ok()
    }

    /// Takes in the chunks given back, gives their buffers to the kernel again once the kernel is
    /// done with them and no chunk of them is out, and counts each chunk off the receive it went
    /// to with `release(user_data, charged)`.
    pub(crate) fn take_back(&mut self, mut release: impl FnMut(u64, usize)) {
        let mut any = false;
        while let Some(Back { bid, owner, len }) =
            self.local.pop().or_else(|| self.back.queue.try_pop().ok())
        {
            if owner != NOT_AN_OP {
                release(owner, self.charge(len));
            }
            #[cfg(test)]
            {
                self.out -= len as usize;
            }
            if let Some(fill) = self.fills.get_mut(usize::from(bid)) {
                fill.live = fill.live.saturating_sub(1);
            }
            any |= self.settle(bid);
        }
        if any {
            self.ring.commit();
        }
    }

    /// Keeps the chunk `item` of `back` to take in with the rest, if `back` is this group's:
    /// for a chunk dropped on the ring's own thread.
    pub(crate) fn keep_local(&mut self, back: &Arc<Returns>, item: Back) -> bool {
        let mine = Arc::ptr_eq(&self.back, back) && self.local.len() < self.local.capacity();
        if mine {
            self.local.push(item);
        }
        mine
    }
}

#[cfg(test)]
impl RecvGroup {
    /// The bytes of the chunks handed out and not given back yet.
    pub(crate) fn out(&self) -> usize {
        self.out
    }

    /// Buffers given to the kernel again so far.
    pub(crate) fn readded(&self) -> usize {
        self.readded
    }

    /// Chunks handed out and not given back yet, over all buffers.
    pub(crate) fn live(&self) -> u32 {
        self.fills.iter().map(|f| f.live).sum()
    }
}

impl RingChunk {
    /// The bytes not taken yet.
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: the buffer was handed out and goes back only when this
        // chunk drops, so the kernel does not write it while it lives.
        unsafe { self.back.mem.bytes(self.bid, self.start, self.end) }.unwrap_or_default()
    }

    /// Keeps only bytes `start..end` of the buffer: the payload of a
    /// message whose header and sender come first.
    pub(crate) fn narrow(&mut self, start: usize, end: usize) {
        let (start, end) = (
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(end).unwrap_or(u32::MAX),
        );
        self.end = end.min(self.end);
        self.start = start.min(self.end);
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

impl RingChunk {
    /// What the chunk counted against its receive's cap when it was handed out.
    pub(crate) fn charged(&self, group: &RecvGroup) -> usize {
        group.charge(self.len)
    }
}

impl Drop for RingChunk {
    fn drop(&mut self) {
        let item = Back {
            bid: self.bid,
            owner: self.owner,
            len: self.len,
        };
        let kept = crate::rt::context::current(|shard| {
            shard
                .and_then(crate::rt::shard::Shard::ring)
                .is_some_and(|ring| ring.keep_local(&self.back, item))
        });
        if !kept {
            self.back.queue.push(item);
        }
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
