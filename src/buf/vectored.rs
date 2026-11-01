//! Owned vectored IO: several of the caller's buffers one call writes from
//! in order ([`IoVecBuf`]) or reads into in order ([`IoVecBufMut`]), given
//! back together with its result.

use core::mem::MaybeUninit;

use super::owned::{IoBuf, IoBufMut, OwnedBuf};

mod sealed {
    use super::OwnedBuf;

    /// The buffers of a vectored call as the runtime holds them.
    pub struct Bufs(pub(crate) Vec<OwnedBuf>);

    /// How the runtime holds the buffers of a vectored write, and gives
    /// them back.
    pub trait Written: Sized {
        fn into_owned(self) -> Bufs;
        fn from_owned(bufs: Bufs) -> Self;
    }

    /// As [`Written`], for a vectored read.
    pub trait Filled: Sized {
        fn into_owned(self) -> Bufs;
        fn from_owned(bufs: Bufs) -> Self;
    }
}

use sealed::Bufs;

/// The buffers of an owned vectored write: each one's bytes, in order, go
/// out as one write. Implemented for `Vec<B>` and `[B; N]` of any [`IoBuf`],
/// `Vec<Vec<u8>>` among them.
pub trait IoVecBuf: sealed::Written + Send + 'static {}

/// The buffers of an owned vectored read: the bytes read fill each one's
/// spare capacity in order. Implemented for `Vec<B>` and `[B; N]` of any
/// [`IoBufMut`].
pub trait IoVecBufMut: sealed::Filled + Send + 'static {}

impl<B> sealed::Written for Vec<B>
where
    B: IoBuf,
{
    fn into_owned(self) -> Bufs {
        Bufs(self.into_iter().map(OwnedBuf::written).collect())
    }

    fn from_owned(bufs: Bufs) -> Self {
        bufs.0.into_iter().map(OwnedBuf::into_written).collect()
    }
}

impl<B> IoVecBuf for Vec<B> where B: IoBuf {}

impl<B> sealed::Filled for Vec<B>
where
    B: IoBufMut,
{
    fn into_owned(self) -> Bufs {
        Bufs(self.into_iter().map(OwnedBuf::filled).collect())
    }

    fn from_owned(bufs: Bufs) -> Self {
        bufs.0.into_iter().map(OwnedBuf::into_filled).collect()
    }
}

impl<B> IoVecBufMut for Vec<B> where B: IoBufMut {}

impl<B, const N: usize> sealed::Written for [B; N]
where
    B: IoBuf,
{
    fn into_owned(self) -> Bufs {
        Bufs(self.into_iter().map(OwnedBuf::written).collect())
    }

    fn from_owned(bufs: Bufs) -> Self {
        let mut bufs = bufs.0.into_iter();
        core::array::from_fn(|_| bufs.next().map_or_else(B::default, OwnedBuf::into_written))
    }
}

impl<B, const N: usize> IoVecBuf for [B; N] where B: IoBuf {}

impl<B, const N: usize> sealed::Filled for [B; N]
where
    B: IoBufMut,
{
    fn into_owned(self) -> Bufs {
        Bufs(self.into_iter().map(OwnedBuf::filled).collect())
    }

    fn from_owned(bufs: Bufs) -> Self {
        let mut bufs = bufs.0.into_iter();
        core::array::from_fn(|_| bufs.next().map_or_else(B::default, OwnedBuf::into_filled))
    }
}

impl<B, const N: usize> IoVecBufMut for [B; N] where B: IoBufMut {}

/// The buffers of a vectored write, as the runtime holds them.
pub(crate) fn written<V>(bufs: V) -> Vec<OwnedBuf>
where
    V: IoVecBuf,
{
    sealed::Written::into_owned(bufs).0
}

/// The caller's buffers of a vectored write back.
pub(crate) fn into_written<V>(bufs: Vec<OwnedBuf>) -> V
where
    V: IoVecBuf,
{
    <V as sealed::Written>::from_owned(Bufs(bufs))
}

/// The buffers of a vectored read, as the runtime holds them.
pub(crate) fn filled<V>(bufs: V) -> Vec<OwnedBuf>
where
    V: IoVecBufMut,
{
    sealed::Filled::into_owned(bufs).0
}

/// The caller's buffers of a vectored read back, holding what was read.
pub(crate) fn into_filled<V>(bufs: Vec<OwnedBuf>) -> V
where
    V: IoVecBufMut,
{
    <V as sealed::Filled>::from_owned(Bufs(bufs))
}

/// The descriptor of one buffer's bytes or room that the kernel is handed:
/// an `iovec` on unix, a `WSABUF` on Windows.
#[cfg(unix)]
pub(crate) type IoVec = libc::iovec;
#[cfg(windows)]
pub(crate) type IoVec = crate::syscore::windows::WSABUF;

/// The most buffers one vectored call hands the kernel: `IOV_MAX` on the
/// unix systems nuclei runs on. A call over more is a short one, which the
/// write goes on from and the read answers as it is.
pub(crate) const IOV_MAX: usize = 1024;

fn iovec(ptr: *const u8, len: usize) -> IoVec {
    #[cfg(unix)]
    return libc::iovec {
        iov_base: ptr.cast_mut().cast(),
        iov_len: len,
    };
    #[cfg(windows)]
    return IoVec {
        len: u32::try_from(len).unwrap_or(u32::MAX),
        buf: ptr.cast_mut(),
    };
}

/// The buffers of a vectored call while the runtime holds them: written
/// from in order, or read into in order, each buffer's room filled before
/// the next one's.
pub(crate) struct Vectored {
    bufs: Vec<OwnedBuf>,
    /// A read's: how many bytes each buffer held when the call took it; the
    /// bytes read land in the room after them.
    held: Option<Vec<usize>>,
    /// The descriptors of the call out now, rebuilt for each call and kept
    /// here, so they stay in place while the kernel reads them.
    iov: Vec<IoVec>,
    /// The message a ring's vectored send names.
    #[cfg(ring)]
    msg: libc::msghdr,
}

// SAFETY: the descriptors and the message point only into the buffers the
// value owns, and are read only by the call that is handed them.
unsafe impl Send for Vectored {}
// SAFETY: as for `Send`: shared, the value is only read.
unsafe impl Sync for Vectored {}

impl Vectored {
    fn new(bufs: Vec<OwnedBuf>, held: Option<Vec<usize>>) -> Self {
        Self {
            bufs,
            held,
            iov: Vec::new(),
            // SAFETY: an all-zero `msghdr` is valid (no name, no control data).
            #[cfg(ring)]
            msg: unsafe { core::mem::zeroed() },
        }
    }

    /// The buffers of a write.
    pub(crate) fn written(bufs: Vec<OwnedBuf>) -> Self {
        Self::new(bufs, None)
    }

    /// The buffers of a read: each one's bytes stay, and its room is filled.
    pub(crate) fn filled(bufs: Vec<OwnedBuf>) -> Self {
        let held = bufs.iter().map(OwnedBuf::len).collect();
        Self::new(bufs, Some(held))
    }

    pub(crate) fn into_bufs(self) -> Vec<OwnedBuf> {
        self.bufs
    }

    /// How many bytes the buffers hold.
    pub(crate) fn len(&self) -> usize {
        self.bufs.iter().map(OwnedBuf::len).sum()
    }

    /// How many bytes the buffers' memory holds: for a write, the bytes they
    /// hold (nothing is read into it).
    pub(crate) fn capacity(&self) -> usize {
        match self.held {
            Some(_) => self.bufs.iter().map(OwnedBuf::capacity).sum(),
            None => self.len(),
        }
    }

    /// The bytes from the `from`th one they hold to the end of the buffer it
    /// is in.
    pub(crate) fn rest_from(&self, from: usize) -> &[u8] {
        let mut from = from;
        for buf in &self.bufs {
            if from < buf.len() {
                return buf.rest_from(from);
            }
            from -= buf.len();
        }
        &[]
    }

    /// The descriptors of the bytes from the `from`th on, in order, for one
    /// vectored write (at most [`IOV_MAX`] of them); empty past the end.
    pub(crate) fn write_iovecs(&mut self, from: usize) -> &[IoVec] {
        self.iov.clear();
        let mut skip = from;
        for buf in &self.bufs {
            if self.iov.len() == IOV_MAX {
                break;
            }
            let bytes = buf.rest_from(skip);
            skip = skip.saturating_sub(buf.len());
            if !bytes.is_empty() {
                self.iov.push(iovec(bytes.as_ptr(), bytes.len()));
            }
        }
        &self.iov
    }

    /// The descriptors of the buffers' room, in order, for one vectored read
    /// (at most [`IOV_MAX`] of them): every buffer before the first one with
    /// room is full.
    pub(crate) fn read_iovecs(&mut self) -> &mut [IoVec] {
        self.iov.clear();
        if self.held.is_some() {
            for buf in &mut self.bufs {
                if self.iov.len() == IOV_MAX {
                    break;
                }
                let room = buf.spare_capacity_mut();
                if !room.is_empty() {
                    self.iov.push(iovec(room.as_mut_ptr().cast(), room.len()));
                }
            }
        }
        &mut self.iov
    }

    /// The message of a ring's vectored send of the bytes from the `from`th
    /// on: the descriptors of [`write_iovecs`](Self::write_iovecs).
    #[cfg(ring)]
    pub(crate) fn send_msg(&mut self, from: usize) -> *const libc::msghdr {
        let count = self.write_iovecs(from).len();
        self.msg.msg_iov = self.iov.as_mut_ptr();
        self.msg.msg_iovlen = count as _;
        &raw const self.msg
    }

    /// The message of a ring's vectored receive into the rooms of
    /// [`read_iovecs`](Self::read_iovecs).
    #[cfg(ring)]
    pub(crate) fn recv_msg(&mut self) -> *mut libc::msghdr {
        let count = self.read_iovecs().len();
        self.msg.msg_iov = self.iov.as_mut_ptr();
        self.msg.msg_iovlen = count as _;
        &raw mut self.msg
    }

    /// The descriptors the last [`write_iovecs`](Self::write_iovecs) or
    /// [`read_iovecs`](Self::read_iovecs) built.
    #[cfg(windows)]
    pub(crate) fn built(&mut self) -> &mut [IoVec] {
        &mut self.iov
    }

    /// The room of the first buffer that has any.
    pub(crate) fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        if self.held.is_none() {
            return &mut [];
        }
        match self.bufs.iter_mut().find(|b| b.len() < b.capacity()) {
            Some(buf) => buf.spare_capacity_mut(),
            None => &mut [],
        }
    }

    /// Sets how many bytes the buffers hold to `len`: the bytes past what
    /// they held when the call took them fill each buffer's room in order.
    ///
    /// # Safety
    ///
    /// `len <= capacity()`, and those bytes were written into the rooms in
    /// that order.
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        let Some(held) = &self.held else {
            return;
        };
        let mut left = len.saturating_sub(held.iter().sum());
        for (buf, &held) in self.bufs.iter_mut().zip(held) {
            let take = left.min(buf.capacity() - held);
            left -= take;
            // SAFETY: the caller's: the first `held + take` bytes of this
            // buffer are initialized, and within its capacity.
            unsafe { buf.set_len(held + take) };
        }
    }

    /// Copies as much of `bytes` as the rooms hold into them in order;
    /// returns how many.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> usize {
        if self.held.is_none() {
            return 0;
        }
        let mut n = 0;
        for buf in &mut self.bufs {
            if n == bytes.len() {
                break;
            }
            n += buf.append(&bytes[n..]);
        }
        n
    }

    /// The buffers of a read whose task went away, each with what it held
    /// before the read: only the buffers that got bytes.
    pub(crate) fn read_parts(self) -> impl Iterator<Item = (OwnedBuf, usize)> {
        let held = self.held.unwrap_or_default();
        self.bufs
            .into_iter()
            .zip(held)
            .filter(|(buf, held)| buf.len() > *held)
    }
}
