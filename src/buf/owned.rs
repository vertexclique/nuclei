//! The caller's own buffers that owned calls read into and write from
//! ([`IoBuf`], [`IoBufMut`]), and the one form the runtime keeps them in
//! while a call has them ([`OwnedBuf`]).
//!
//! An owned call takes the caller's buffer by value and gives it back with
//! its result, so the kernel reads or writes the caller's memory and no
//! byte is copied. While the call has the buffer, the runtime holds it as
//! an [`OwnedBuf`]: a `Vec<u8>` or an [`AlignedBuf`] as it is, any other type
//! boxed once. The call's code then works on that one type wherever the
//! buffer goes (a ring operation, a job of the blocking pool, a side's lent
//! read or write), and the buffer comes back as the caller's own type.
//!
//! A buffer that cannot come back (the call failed while the kernel still
//! held it, so the runtime keeps it until the kernel is done) is answered
//! with an empty one, its type's [`Default`].

use core::any::{Any, TypeId};
use core::mem::{ManuallyDrop, MaybeUninit};
use core::panic::AssertUnwindSafe;

use super::aligned::AlignedBuf;

/// A buffer an owned write takes: memory at an address that does not move
/// while the runtime holds the buffer, whose first
/// [`bytes_init`](Self::bytes_init) bytes are written.
///
/// Implemented for `Vec<u8>` and [`AlignedBuf`]; a caller's own type (a
/// shared byte buffer, memory of a registered region) may implement it too.
/// It is `Send` and `Sync`, since the runtime may hand it to another thread
/// and several zero-copy sends read it at once, and its [`Default`] is the
/// empty buffer a call gives back when the caller's own cannot come back.
///
/// # Safety
///
/// [`stable_ptr`](Self::stable_ptr) is never null and points at
/// [`bytes_total`](Self::bytes_total) bytes of memory, the first [`bytes_init`](Self::bytes_init) of them
/// initialized, which stay valid and in place while the value is not used
/// through `&mut` or dropped, even when the value itself moves: the kernel
/// may read them after the value moved into the runtime.
/// `bytes_init() <= bytes_total()`.
pub unsafe trait IoBuf: Default + Send + Sync + 'static {
    /// The address of the buffer's first byte.
    fn stable_ptr(&self) -> *const u8;

    /// How many bytes from the start hold data: what a write sends.
    fn bytes_init(&self) -> usize;

    /// How many bytes the buffer's memory holds in all.
    fn bytes_total(&self) -> usize;
}

/// A buffer an owned read fills: an [`IoBuf`] whose memory past the bytes
/// it holds may be written, and told how many hold data after a read.
///
/// # Safety
///
/// As [`IoBuf`]; and [`stable_mut_ptr`](Self::stable_mut_ptr) points at the
/// same memory, writable for all [`bytes_total`](IoBuf::bytes_total) bytes.
pub unsafe trait IoBufMut: IoBuf {
    /// The address of the buffer's first byte, for writing.
    fn stable_mut_ptr(&mut self) -> *mut u8;

    /// Sets how many bytes from the start hold data to `pos` (the runtime
    /// calls it with a smaller count too, never below what the caller
    /// handed in).
    ///
    /// # Safety
    ///
    /// `pos <= bytes_total()`, and the first `pos` bytes are initialized.
    unsafe fn set_init(&mut self, pos: usize);
}

// SAFETY: a vector's heap memory does not move with the vector, and its
// first `len` bytes of `capacity` are initialized.
unsafe impl IoBuf for Vec<u8> {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }

    fn bytes_total(&self) -> usize {
        self.capacity()
    }
}

// SAFETY: as for `IoBuf`; the whole capacity is writable.
unsafe impl IoBufMut for Vec<u8> {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: the caller's: `pos` is within the capacity and initialized.
        unsafe { self.set_len(pos) }
    }
}

/// `value` as a `U`, when `T` is `U`.
fn cast<T, U>(value: T) -> Result<U, T>
where
    T: 'static,
    U: 'static,
{
    if TypeId::of::<T>() != TypeId::of::<U>() {
        return Err(value);
    }
    let value = ManuallyDrop::new(value);
    // SAFETY: `T` and `U` are one type, and the value is never dropped as a
    // `T`, so it is moved, not copied.
    Ok(unsafe { core::ptr::read((&raw const *value).cast::<U>()) })
}

/// A caller's buffer of a type of its own, boxed while the runtime holds it.
pub(crate) trait Kept: Any + Send + Sync {
    fn ptr(&self) -> *const u8;
    fn mut_ptr(&mut self) -> *mut u8;
    fn init(&self) -> usize;
    fn total(&self) -> usize;
    /// # Safety
    ///
    /// As [`IoBufMut::set_init`].
    unsafe fn set_init(&mut self, pos: usize);
}

/// The buffer of a write: nothing is ever read into it, so its room past
/// the bytes it holds is none.
struct Written<B>(B);

impl<B> Kept for Written<B>
where
    B: IoBuf,
{
    fn ptr(&self) -> *const u8 {
        self.0.stable_ptr()
    }

    fn mut_ptr(&mut self) -> *mut u8 {
        // Never written through: the buffer has no room past its bytes.
        self.0.stable_ptr().cast_mut()
    }

    fn init(&self) -> usize {
        self.0.bytes_init()
    }

    fn total(&self) -> usize {
        self.0.bytes_init()
    }

    unsafe fn set_init(&mut self, _pos: usize) {}
}

/// The buffer of a read.
struct Filled<B>(B);

impl<B> Kept for Filled<B>
where
    B: IoBufMut,
{
    fn ptr(&self) -> *const u8 {
        self.0.stable_ptr()
    }

    fn mut_ptr(&mut self) -> *mut u8 {
        self.0.stable_mut_ptr()
    }

    fn init(&self) -> usize {
        self.0.bytes_init()
    }

    fn total(&self) -> usize {
        self.0.bytes_total()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: the caller's.
        unsafe { self.0.set_init(pos) }
    }
}

/// A caller's buffer while an owned call holds it: what the call reads into
/// or writes from, in one type wherever the call goes. It reads as a `Vec<u8>`
/// does: its bytes, its spare capacity, and the count of bytes it holds.
///
/// It is as large as a `Vec<u8>`, so the operations that carry it are no
/// larger than they were for a vector: any other buffer is boxed.
pub(crate) enum OwnedBuf {
    Vec(Vec<u8>),
    Boxed(Box<Boxed>),
}

const _: () = assert!(size_of::<OwnedBuf>() == size_of::<Vec<u8>>());

/// A buffer of another type than `Vec<u8>`.
pub(crate) enum Boxed {
    Aligned(AlignedBuf),
    /// Unwind safe as a `Vec<u8>` is, since a panic leaves no invariant of
    /// the caller's buffer half made that the runtime relies on.
    Kept(AssertUnwindSafe<Box<dyn Kept>>),
    /// The buffers of a vectored write, written one after the other: their
    /// bytes are not one slice, so its `Deref` is empty and its users read
    /// it through [`OwnedBuf::rest_from`] and [`OwnedBuf::len`].
    Vectored(Vec<OwnedBuf>),
}

impl Default for OwnedBuf {
    fn default() -> Self {
        OwnedBuf::Vec(Vec::new())
    }
}

impl OwnedBuf {
    #[inline]
    fn boxed(boxed: Boxed) -> Self {
        OwnedBuf::Boxed(Box::new(boxed))
    }

    /// The buffer of a write; a `Vec<u8>` as it is.
    #[inline]
    pub(crate) fn written<B>(buf: B) -> Self
    where
        B: IoBuf,
    {
        let buf = match cast::<B, Vec<u8>>(buf) {
            Ok(vec) => return OwnedBuf::Vec(vec),
            Err(buf) => buf,
        };
        let buf = match cast::<B, OwnedBuf>(buf) {
            Ok(owned) => return owned,
            Err(buf) => buf,
        };
        match cast::<B, AlignedBuf>(buf) {
            Ok(aligned) => Self::boxed(Boxed::Aligned(aligned)),
            Err(buf) => Self::boxed(Boxed::Kept(AssertUnwindSafe(Box::new(Written(buf))))),
        }
    }

    /// The buffer of a read; a `Vec<u8>` as it is.
    #[inline]
    pub(crate) fn filled<B>(buf: B) -> Self
    where
        B: IoBufMut,
    {
        let buf = match cast::<B, Vec<u8>>(buf) {
            Ok(vec) => return OwnedBuf::Vec(vec),
            Err(buf) => buf,
        };
        let buf = match cast::<B, OwnedBuf>(buf) {
            Ok(owned) => return owned,
            Err(buf) => buf,
        };
        match cast::<B, AlignedBuf>(buf) {
            Ok(aligned) => Self::boxed(Boxed::Aligned(aligned)),
            Err(buf) => Self::boxed(Boxed::Kept(AssertUnwindSafe(Box::new(Filled(buf))))),
        }
    }

    /// The caller's buffer of a write back, as the type it was handed in
    /// as; an empty one if the call could not give it back.
    #[inline]
    pub(crate) fn into_written<B>(self) -> B
    where
        B: IoBuf,
    {
        match self {
            OwnedBuf::Vec(vec) => match cast::<Vec<u8>, B>(vec) {
                Ok(same) => same,
                Err(vec) => OwnedBuf::Vec(vec).into_other::<B, Written<B>>(|kept| kept.0),
            },
            other => other.into_other::<B, Written<B>>(|kept| kept.0),
        }
    }

    /// The caller's buffer of a read back, holding the bytes read into it.
    #[inline]
    pub(crate) fn into_filled<B>(self) -> B
    where
        B: IoBufMut,
    {
        match self {
            OwnedBuf::Vec(vec) => match cast::<Vec<u8>, B>(vec) {
                Ok(same) => same,
                Err(vec) => OwnedBuf::Vec(vec).into_other::<B, Filled<B>>(|kept| kept.0),
            },
            other => other.into_other::<B, Filled<B>>(|kept| kept.0),
        }
    }

    /// The caller's buffer back when it is not a `Vec<u8>`: the runtime's own
    /// form as it is, a boxed one unboxed, or an empty one if it cannot.
    #[inline(never)]
    fn into_other<B, K>(self, unwrap: fn(K) -> B) -> B
    where
        B: IoBuf,
        K: Kept,
    {
        let boxed = match cast::<OwnedBuf, B>(self) {
            Ok(same) => return same,
            Err(OwnedBuf::Boxed(boxed)) => boxed,
            Err(OwnedBuf::Vec(_)) => return B::default(),
        };
        match *boxed {
            Boxed::Aligned(aligned) => cast::<AlignedBuf, B>(aligned).unwrap_or_default(),
            Boxed::Kept(AssertUnwindSafe(kept)) => {
                let kept: Box<dyn Any> = kept;
                kept.downcast::<K>()
                    .map_or_else(|_| B::default(), |k| unwrap(*k))
            }
            Boxed::Vectored(_) => B::default(),
        }
    }

    /// The buffers of a vectored write, held as one.
    pub(crate) fn vectored(bufs: Vec<OwnedBuf>) -> Self {
        Self::boxed(Boxed::Vectored(bufs))
    }

    /// The buffers of a vectored write back; a buffer of one is its own.
    pub(crate) fn into_segments(self) -> Vec<OwnedBuf> {
        match self {
            OwnedBuf::Boxed(boxed) => match *boxed {
                Boxed::Vectored(bufs) => bufs,
                one => vec![OwnedBuf::Boxed(Box::new(one))],
            },
            one => vec![one],
        }
    }

    /// The bytes from the `from`th of those it holds to the end of the slice
    /// that byte is in: all the rest for one buffer, the rest of the buffer
    /// it falls in for a vectored write. Empty past its end.
    #[inline]
    pub(crate) fn rest_from(&self, from: usize) -> &[u8] {
        match self {
            OwnedBuf::Vec(vec) => vec.get(from..).unwrap_or_default(),
            OwnedBuf::Boxed(boxed) => boxed.rest_from(from),
        }
    }

    /// How many bytes it holds.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self {
            OwnedBuf::Vec(vec) => vec.len(),
            OwnedBuf::Boxed(boxed) => boxed.len(),
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many bytes its memory holds in all (for a write's buffer, the
    /// bytes it holds).
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        match self {
            OwnedBuf::Vec(vec) => vec.capacity(),
            OwnedBuf::Boxed(boxed) => boxed.capacity(),
        }
    }

    #[inline]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            OwnedBuf::Vec(vec) => vec.as_mut_ptr(),
            OwnedBuf::Boxed(boxed) => boxed.as_mut_ptr(),
        }
    }

    /// Its memory past the bytes it holds.
    #[inline]
    pub(crate) fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            OwnedBuf::Vec(vec) => vec.spare_capacity_mut(),
            OwnedBuf::Boxed(boxed) => boxed.spare_capacity_mut(),
        }
    }

    /// Sets how many bytes it holds to `len`.
    ///
    /// # Safety
    ///
    /// `len <= capacity()`, and the first `len` bytes are initialized.
    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        match self {
            // SAFETY: the caller's.
            OwnedBuf::Vec(vec) => unsafe { vec.set_len(len) },
            // SAFETY: the caller's.
            OwnedBuf::Boxed(boxed) => unsafe { boxed.set_len(len) },
        }
    }

    /// Keeps only its first `len` bytes.
    #[inline]
    pub(crate) fn truncate(&mut self, len: usize) {
        if len < self.len() {
            // SAFETY: fewer bytes than it holds, so all initialized.
            unsafe { self.set_len(len) };
        }
    }

    /// Copies as much of `bytes` as its spare capacity holds onto its end;
    /// returns how many.
    #[inline]
    pub(crate) fn append(&mut self, bytes: &[u8]) -> usize {
        let len = self.len();
        let spare = self.spare_capacity_mut();
        let n = bytes.len().min(spare.len());
        for (to, from) in spare.iter_mut().zip(&bytes[..n]) {
            to.write(*from);
        }
        // SAFETY: the `n` bytes after the first `len` were just written.
        unsafe { self.set_len(len + n) };
        n
    }

    /// Zeroes its spare capacity and lends it as bytes; how many it holds is
    /// left as it was.
    pub(crate) fn spare_zeroed(&mut self) -> &mut [u8] {
        let len = self.len();
        let room = self.capacity().saturating_sub(len);
        if room == 0 {
            return &mut [];
        }
        let ptr = self.as_mut_ptr();
        // SAFETY: the `room` bytes past the first `len` are the buffer's own
        // writable memory; zeroed, they are initialized.
        unsafe {
            core::ptr::write_bytes(ptr.add(len), 0, room);
            core::slice::from_raw_parts_mut(ptr.add(len), room)
        }
    }
}

impl core::ops::Deref for OwnedBuf {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            OwnedBuf::Vec(vec) => vec,
            OwnedBuf::Boxed(boxed) => boxed.bytes(),
        }
    }
}

/// The buffers of other types than `Vec<u8>`, kept out of line so that a
/// vector's accessors stay small enough to inline.
impl Boxed {
    #[inline(never)]
    fn len(&self) -> usize {
        match self {
            Boxed::Aligned(aligned) => aligned.len(),
            Boxed::Kept(kept) => kept.init(),
            Boxed::Vectored(bufs) => bufs.iter().map(OwnedBuf::len).sum(),
        }
    }

    #[inline(never)]
    fn capacity(&self) -> usize {
        match self {
            Boxed::Aligned(aligned) => aligned.capacity(),
            Boxed::Kept(kept) => kept.total(),
            Boxed::Vectored(_) => self.len(),
        }
    }

    #[inline(never)]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            Boxed::Aligned(aligned) => aligned.as_mut_ptr(),
            Boxed::Kept(kept) => kept.mut_ptr(),
            Boxed::Vectored(_) => core::ptr::NonNull::dangling().as_ptr(),
        }
    }

    #[inline(never)]
    fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        match self {
            Boxed::Aligned(aligned) => aligned.spare_capacity_mut(),
            Boxed::Kept(kept) => {
                let (len, total) = (kept.init(), kept.total());
                if len >= total {
                    return &mut [];
                }
                // SAFETY: the buffer's memory holds `total` bytes, writable
                // past the `len` that hold data (a write's has none).
                unsafe {
                    core::slice::from_raw_parts_mut(
                        kept.mut_ptr().add(len).cast::<MaybeUninit<u8>>(),
                        total - len,
                    )
                }
            }
            Boxed::Vectored(_) => &mut [],
        }
    }

    /// # Safety
    ///
    /// As [`OwnedBuf::set_len`].
    #[inline(never)]
    unsafe fn set_len(&mut self, len: usize) {
        match self {
            // SAFETY: the caller's.
            Boxed::Aligned(aligned) => unsafe { aligned.set_len(len) },
            // SAFETY: the caller's.
            Boxed::Kept(kept) => unsafe { kept.set_init(len) },
            // A write's: nothing is read into it.
            Boxed::Vectored(_) => {}
        }
    }

    #[inline(never)]
    fn bytes(&self) -> &[u8] {
        match self {
            Boxed::Aligned(aligned) => aligned,
            Boxed::Kept(kept) if kept.init() == 0 => &[],
            // SAFETY: the first `init` bytes of the buffer's memory hold
            // data.
            Boxed::Kept(kept) => unsafe { core::slice::from_raw_parts(kept.ptr(), kept.init()) },
            Boxed::Vectored(_) => &[],
        }
    }

    #[inline(never)]
    fn rest_from(&self, from: usize) -> &[u8] {
        let Boxed::Vectored(bufs) = self else {
            return self.bytes().get(from..).unwrap_or_default();
        };
        let mut from = from;
        for buf in bufs {
            if from < buf.len() {
                return buf.rest_from(from);
            }
            from -= buf.len();
        }
        &[]
    }
}

impl core::fmt::Debug for OwnedBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OwnedBuf")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

// SAFETY: the runtime's own form of a caller's buffer, passed through the
// owned calls as it is (never through these methods but by the runtime's
// own code, which reads a vectored write through `rest_from`).
unsafe impl IoBuf for OwnedBuf {
    fn stable_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }

    fn bytes_total(&self) -> usize {
        self.capacity()
    }
}

// SAFETY: as for `IoBuf`.
unsafe impl IoBufMut for OwnedBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: the caller's.
        unsafe { self.set_len(pos) }
    }
}
