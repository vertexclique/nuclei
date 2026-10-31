//! Owned vectored IO: several of the caller's buffers one call writes from
//! in order ([`IoVecBuf`]) or reads into in order ([`IoVecBufMut`]), given
//! back together with its result.

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
