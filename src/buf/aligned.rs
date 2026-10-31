//! [`AlignedBuf`]: a buffer whose memory starts at an address of the
//! caller's chosen alignment, for a file opened for direct IO.

use core::mem::MaybeUninit;
use core::ptr::NonNull;
use std::alloc::{self, Layout};
use std::io;

use super::owned::{IoBuf, IoBufMut};

/// A buffer of a fixed capacity whose memory starts at an address that is
/// a multiple of a chosen alignment: the buffer a file opened for direct IO
/// (`O_DIRECT`, `FILE_FLAG_NO_BUFFERING`) is read into and written from by
/// the owned calls ([`Handle::read_at_owned`](crate::io::Handle::read_at_owned),
/// [`Handle::write_at_owned`](crate::io::Handle::write_at_owned) and the
/// others), which hand its memory to the kernel as it is.
///
/// The kernel's alignment rules for direct IO ask the memory's address, the
/// file offset and the length to be multiples of the device's logical block
/// size (512 or 4096 bytes on most devices); a buffer aligned to 4096 with a
/// capacity of a multiple of 4096 meets them on all of them.
///
/// It holds its first [`len`](Self::len) bytes, as a `Vec<u8>` does, and
/// never grows past its capacity. Its memory is the caller's: it is not
/// charged to the runtime's IO buffer cap.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: the buffer owns its memory, which nothing else points at.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared, it is only read.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// An empty buffer with room for `capacity` bytes, its memory aligned to
    /// `align` bytes.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when `align` is not a power of two, or `capacity`
    /// rounded up to it is past what memory can hold; `OutOfMemory` when
    /// the allocator has no memory for it.
    pub fn new(capacity: usize, align: usize) -> io::Result<Self> {
        Self::alloc(capacity, align, false)
    }

    /// A buffer of `len` zeroed bytes, its memory aligned to `align` bytes,
    /// with room for exactly `len`.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn zeroed(len: usize, align: usize) -> io::Result<Self> {
        let mut buf = Self::alloc(len, align, true)?;
        buf.len = len;
        Ok(buf)
    }

    fn alloc(capacity: usize, align: usize, zeroed: bool) -> io::Result<Self> {
        if !align.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the alignment is not a power of two",
            ));
        }
        let layout = Layout::from_size_align(capacity, align).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the buffer is larger than memory can hold",
            )
        })?;
        if capacity == 0 {
            return Ok(Self {
                // An address of the alignment, never read or written through.
                ptr: NonNull::new(core::ptr::without_provenance_mut(align))
                    .unwrap_or(NonNull::dangling()),
                len: 0,
                layout,
            });
        }
        // SAFETY: the layout's size is not zero.
        let raw = unsafe {
            if zeroed {
                alloc::alloc_zeroed(layout)
            } else {
                alloc::alloc(layout)
            }
        };
        let ptr = NonNull::new(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "the allocator has no memory for the buffer",
            )
        })?;
        Ok(Self {
            ptr,
            len: 0,
            layout,
        })
    }

    /// How many bytes it holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds no byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many bytes its memory has room for.
    pub fn capacity(&self) -> usize {
        self.layout.size()
    }

    /// The alignment of its memory's address.
    pub fn alignment(&self) -> usize {
        self.layout.align()
    }

    /// The address of its first byte.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// The address of its first byte, for writing.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Holds no byte any more; the memory stays.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Keeps only its first `len` bytes (all of them if it holds fewer).
    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    /// Copies as much of `bytes` as its room holds onto its end and returns
    /// how many: it never grows past its capacity.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.capacity() - self.len);
        // SAFETY: the `n` bytes after the first `len` are within the
        // buffer's memory, which `bytes` cannot overlap (it is borrowed
        // apart from `self`).
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(self.len), n);
        }
        self.len += n;
        n
    }

    /// Its memory past the bytes it holds.
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        // SAFETY: the memory holds `capacity` bytes, the buffer's own.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.ptr.as_ptr().add(self.len).cast::<MaybeUninit<u8>>(),
                self.capacity() - self.len,
            )
        }
    }

    /// Sets how many bytes it holds to `len`.
    ///
    /// # Safety
    ///
    /// `len <= capacity()`, and the first `len` bytes are initialized.
    pub unsafe fn set_len(&mut self, len: usize) {
        self.len = len;
    }
}

impl Default for AlignedBuf {
    /// An empty buffer with no memory.
    fn default() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            layout: Layout::new::<()>(),
        }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.layout.size() != 0 {
            // SAFETY: allocated with this layout by `alloc`, freed once.
            unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }
}

impl core::ops::Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: the first `len` bytes are initialized.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl core::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `deref`, and borrowed uniquely.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl core::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("capacity", &self.capacity())
            .field("alignment", &self.alignment())
            .finish()
    }
}

// SAFETY: the memory is the buffer's own heap allocation, which does not
// move with the value; its first `len` bytes are initialized.
unsafe impl IoBuf for AlignedBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len
    }

    fn bytes_total(&self) -> usize {
        self.capacity()
    }
}

// SAFETY: as for `IoBuf`; the whole capacity is writable.
unsafe impl IoBufMut for AlignedBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        self.len = pos;
    }
}
