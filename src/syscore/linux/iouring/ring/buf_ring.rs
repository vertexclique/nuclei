//! Provided buffer rings: a pool of equally sized buffers the kernel picks from when a receive
//! asks for a buffer group, instead of the caller reserving a buffer per pending receive.
//!
//! The ring of descriptors and the buffers live in application-allocated anonymous mappings
//! (never kernel-allocated ones). The application owns the tail: it writes descriptors with
//! [`BufRing::add`] and publishes them with [`BufRing::commit`]; the kernel owns the head and
//! hands a buffer back through a completion that carries its id ([`BufRing::buffer_id`]), after
//! which the application reads it with [`BufRing::buffer`] and re-adds it.
//!
//! The ring tail is not a field of its own: it overlays the `resv` field of descriptor 0, so
//! descriptor writes touch `addr`, `len` and `bid` only and never `resv`.

use std::{
    ffi::c_void,
    io,
    mem::{ManuallyDrop, size_of},
    os::fd::{AsRawFd, OwnedFd},
    ptr::{self, addr_of_mut},
    slice,
    sync::atomic::{AtomicU16, Ordering},
};

use super::{
    Ring,
    sys::{
        IORING_REGISTER_PBUF_RING, IORING_UNREGISTER_PBUF_RING, IoUringBuf, IoUringBufReg, Mapping,
        cqe_buffer_id, io_uring_register,
    },
};

/// Largest number of descriptors the kernel accepts in one buffer ring.
const MAX_ENTRIES: u16 = 1 << 15;
/// Byte offset of the tail inside descriptor 0 (the `resv` field).
const TAIL_OFFSET: usize = 14;

/// A registered provided-buffer ring and the buffers behind it; one buffer per descriptor slot,
/// buffer id `n` being the `n`-th `buf_size` chunk of the buffer mapping.
#[derive(Debug)]
pub(crate) struct BufRing {
    /// Private duplicate of the ring fd: unregistering works however the `Ring` is dropped.
    ring_fd: OwnedFd,
    bgid: u16,
    entries: u16,
    buf_size: u32,
    /// Tail the kernel can see.
    tail: u16,
    /// Descriptors written since the last commit.
    pending: u16,
    /// The descriptor ring, then the buffers. Left mapped (leaked) if unregistering fails.
    maps: ManuallyDrop<(Mapping, Mapping)>,
}

impl BufRing {
    /// Allocates `entries` buffers of `buf_size` bytes (`entries` a power of two up to 32768),
    /// registers them as group `bgid` on `ring` and returns the empty-looking ring: call
    /// [`BufRing::add`] for each buffer id, then [`BufRing::commit`], before arming a receive.
    pub(crate) fn new(ring: &Ring, bgid: u16, entries: u16, buf_size: u32) -> io::Result<BufRing> {
        if !entries.is_power_of_two() || entries > MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a buffer ring needs a power-of-two size of at most 32768",
            ));
        }
        if buf_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffers need at least one byte",
            ));
        }
        let buf_bytes = usize::from(entries)
            .checked_mul(buf_size as usize)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "the buffer pool is too large")
            })?;
        let ring_map = Mapping::anon(usize::from(entries) * size_of::<IoUringBuf>())?;
        let buf_map = Mapping::anon(buf_bytes)?;
        let ring_fd = ring.fd.try_clone()?;
        let reg = IoUringBufReg {
            ring_addr: ring_map.as_ptr().expose_provenance() as u64,
            ring_entries: u32::from(entries),
            bgid,
            ..IoUringBufReg::default()
        };
        // SAFETY: `reg` is a valid `io_uring_buf_reg` and `ring_map` is the page-aligned,
        // zeroed descriptor ring it names, which stays mapped for as long as it is registered.
        unsafe {
            io_uring_register(
                ring_fd.as_raw_fd(),
                IORING_REGISTER_PBUF_RING,
                ptr::from_ref(&reg).cast(),
                1,
            )
        }?;
        Ok(Self {
            ring_fd,
            bgid,
            entries,
            buf_size,
            tail: 0,
            pending: 0,
            maps: ManuallyDrop::new((ring_map, buf_map)),
        })
    }

    /// The buffer group id receives name to draw from this ring.
    pub(crate) fn bgid(&self) -> u16 {
        self.bgid
    }

    /// Number of buffers in the pool.
    pub(crate) fn entries(&self) -> u16 {
        self.entries
    }

    /// Size in bytes of each buffer.
    pub(crate) fn buf_size(&self) -> u32 {
        self.buf_size
    }

    /// The buffer id a completion used, from its `flags`; `None` when it used no provided buffer.
    pub(crate) fn buffer_id(cqe_flags: u32) -> Option<u16> {
        cqe_buffer_id(cqe_flags)
    }

    fn tail_view(&self) -> &AtomicU16 {
        // SAFETY: byte 14 of descriptor 0 is 2-aligned inside the live descriptor ring mapping,
        // and the kernel reads it only as the ring tail.
        unsafe { &*self.maps.0.as_ptr().add(TAIL_OFFSET).cast::<AtomicU16>() }
    }

    /// Queues buffer `bid` (below the ring size) for the kernel to use; it becomes visible at the
    /// next [`BufRing::commit`]. Fails when `bid` is out of range or the ring already holds as
    /// many uncommitted descriptors as it has slots.
    pub(crate) fn add(&mut self, bid: u16) -> io::Result<()> {
        if bid >= self.entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the buffer id is outside the pool",
            ));
        }
        if self.pending >= self.entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "more buffers were added than the ring holds",
            ));
        }
        let slot = self.tail.wrapping_add(self.pending) & (self.entries - 1);
        let buf = self
            .maps
            .1
            .as_ptr()
            .wrapping_add(usize::from(bid) * self.buf_size as usize);
        // SAFETY: `slot` is below `entries`, so the descriptor lies inside the ring mapping. Only
        // `addr`, `len` and `bid` are written: `resv` of descriptor 0 is the live ring tail.
        unsafe {
            let desc = self
                .maps
                .0
                .as_ptr()
                .cast::<IoUringBuf>()
                .add(usize::from(slot));
            addr_of_mut!((*desc).addr).write(buf.expose_provenance() as u64);
            addr_of_mut!((*desc).len).write(self.buf_size);
            addr_of_mut!((*desc).bid).write(bid);
        }
        self.pending += 1;
        Ok(())
    }

    /// Publishes every descriptor added since the last commit to the kernel.
    pub(crate) fn commit(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.tail = self.tail.wrapping_add(self.pending);
        self.pending = 0;
        // Release: the descriptor writes must be visible before the kernel can see the new tail.
        self.tail_view().store(self.tail, Ordering::Release);
    }

    /// The first `len` bytes of buffer `bid`, or `None` when `bid` or `len` is out of range.
    ///
    /// Call it with the id and length of a completion, before re-adding that id: while a buffer
    /// is queued the kernel may write it. `add` needs `&mut self`, so the borrow checker keeps
    /// the returned slice from outliving a re-add.
    pub(crate) fn buffer(&self, bid: u16, len: u32) -> Option<&[u8]> {
        if bid >= self.entries || len > self.buf_size {
            return None;
        }
        let start = usize::from(bid) * self.buf_size as usize;
        // SAFETY: `start + len` lies inside the buffer mapping (checked above), which lives as
        // long as `self`; a completion for this id means the kernel finished writing it.
        Some(unsafe { slice::from_raw_parts(self.maps.1.as_ptr().add(start), len as usize) })
    }
}

#[cfg(test)]
impl BufRing {
    /// The ring tail as the kernel reads it.
    pub(super) fn published_tail(&self) -> u16 {
        // Acquire: pairs with the release store in `commit`, as the kernel's own read does.
        self.tail_view().load(Ordering::Acquire)
    }

    /// Descriptor `slot` as stored in the shared ring.
    pub(super) fn descriptor(&self, slot: u16) -> IoUringBuf {
        assert!(slot < self.entries);
        // SAFETY: `slot` is below `entries`, so the descriptor lies inside the live ring mapping.
        unsafe {
            ptr::read(
                self.maps
                    .0
                    .as_ptr()
                    .cast::<IoUringBuf>()
                    .add(usize::from(slot)),
            )
        }
    }
}

impl Drop for BufRing {
    /// Unregisters the group, then unmaps. Drop it only after every operation that selects from
    /// the group has completed: the kernel writes into the buffers until then.
    fn drop(&mut self) {
        let reg = IoUringBufReg {
            bgid: self.bgid,
            ..IoUringBufReg::default()
        };
        // SAFETY: `reg` is a valid `io_uring_buf_reg` naming the group registered in `new`.
        let unregistered = unsafe {
            io_uring_register(
                self.ring_fd.as_raw_fd(),
                IORING_UNREGISTER_PBUF_RING,
                ptr::from_ref(&reg).cast::<c_void>(),
                1,
            )
        };
        // vertexia: a failed unregister leaks the two mappings (the kernel may still write the
        // buffers, and unmapping would let their addresses be reused); the fix is a ring-wide
        // quiesce before drop once the op slab can name the groups still in use.
        if unregistered.is_ok() {
            // SAFETY: the group is gone, so the kernel no longer touches either mapping, and
            // `maps` is not used again.
            unsafe { ManuallyDrop::drop(&mut self.maps) };
        }
    }
}
