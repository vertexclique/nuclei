//! [`ReadBuf`]: a buffer a read fills from the front, over memory that may be
//! uninitialized, keeping count of the bytes filled and of those known to be
//! initialized (`filled <= initialized <= capacity`).

use core::fmt;
use core::mem::MaybeUninit;

/// A buffer being filled by reads: its first `filled` bytes hold data, its
/// first `initialized` bytes are initialized (`filled <= initialized`), and
/// the rest may be uninitialized memory that a read may write into.
pub struct ReadBuf<'a> {
    buf: &'a mut [MaybeUninit<u8>],
    filled: usize,
    initialized: usize,
}

impl<'a> ReadBuf<'a> {
    /// A buffer over `buf`, all of it initialized, none of it filled.
    pub fn new(buf: &'a mut [u8]) -> Self {
        let initialized = buf.len();
        // SAFETY: an initialized `u8` is a valid `MaybeUninit<u8>`, and the
        // buffer never writes uninitialized bytes into it (`initialized`
        // starts at the whole length and never decreases).
        let buf = unsafe { &mut *(core::ptr::from_mut::<[u8]>(buf) as *mut [MaybeUninit<u8>]) };
        Self {
            buf,
            filled: 0,
            initialized,
        }
    }

    /// A buffer over `buf`, none of it initialized or filled.
    pub fn uninit(buf: &'a mut [MaybeUninit<u8>]) -> Self {
        Self {
            buf,
            filled: 0,
            initialized: 0,
        }
    }

    /// The buffer's whole length.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// The bytes filled so far.
    pub fn filled(&self) -> &[u8] {
        // SAFETY: the first `filled` bytes are initialized (`filled <=
        // initialized`).
        unsafe { slice_assume_init(&self.buf[..self.filled]) }
    }

    /// The bytes filled so far, to change.
    pub fn filled_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `filled`.
        unsafe { slice_assume_init_mut(&mut self.buf[..self.filled]) }
    }

    /// The bytes known to be initialized, filled or not.
    pub fn initialized(&self) -> &[u8] {
        // SAFETY: the first `initialized` bytes are initialized.
        unsafe { slice_assume_init(&self.buf[..self.initialized]) }
    }

    /// The bytes known to be initialized, to change.
    pub fn initialized_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `initialized`.
        unsafe { slice_assume_init_mut(&mut self.buf[..self.initialized]) }
    }

    /// A buffer over at most `n` bytes of the unfilled part, sharing what is
    /// known to be initialized there.
    pub fn take(&mut self, n: usize) -> ReadBuf<'_> {
        let n = n.min(self.remaining());
        let initialized = self.initialized.saturating_sub(self.filled).min(n);
        let mut taken = ReadBuf::uninit(&mut self.unfilled_uninit()[..n]);
        taken.initialized = initialized;
        taken
    }

    /// Initializes the whole unfilled part (zeroing what was not
    /// initialized) and answers it, for a read to write into.
    pub fn initialize_unfilled(&mut self) -> &mut [u8] {
        self.initialize_unfilled_to(self.remaining())
    }

    /// Initializes the first `n` bytes of the unfilled part and answers them.
    ///
    /// # Panics
    ///
    /// When `n` is more than [`remaining`](Self::remaining).
    pub fn initialize_unfilled_to(&mut self, n: usize) -> &mut [u8] {
        assert!(
            n <= self.remaining(),
            "n overflows the buffer's remaining room"
        );
        let end = self.filled + n;
        if self.initialized < end {
            for byte in &mut self.buf[self.initialized..end] {
                byte.write(0);
            }
            self.initialized = end;
        }
        // SAFETY: `filled..end` was initialized just above or before.
        unsafe { slice_assume_init_mut(&mut self.buf[self.filled..end]) }
    }

    /// The room left after the filled part.
    pub fn remaining(&self) -> usize {
        self.capacity() - self.filled
    }

    /// Empties the filled part; what was initialized stays initialized.
    pub fn clear(&mut self) {
        self.filled = 0;
    }

    /// Counts `n` more bytes as filled.
    ///
    /// # Panics
    ///
    /// When that passes the initialized part.
    pub fn advance(&mut self, n: usize) {
        let filled = self.filled.checked_add(n).expect("filled overflows");
        self.set_filled(filled);
    }

    /// Sets how many bytes are filled.
    ///
    /// # Panics
    ///
    /// When `n` passes the initialized part.
    pub fn set_filled(&mut self, n: usize) {
        assert!(
            n <= self.initialized,
            "filled must not pass the initialized part of the buffer"
        );
        self.filled = n;
    }

    /// Counts the first `n` bytes of the unfilled part as initialized.
    ///
    /// # Safety
    ///
    /// The caller has initialized those bytes.
    pub unsafe fn assume_init(&mut self, n: usize) {
        let end = self.filled + n;
        if end > self.initialized {
            self.initialized = end;
        }
    }

    /// Appends `bytes` to the filled part.
    ///
    /// # Panics
    ///
    /// When they do not fit in [`remaining`](Self::remaining).
    pub fn put_slice(&mut self, bytes: &[u8]) {
        assert!(
            self.remaining() >= bytes.len(),
            "the bytes do not fit in the buffer's remaining room"
        );
        let end = self.filled + bytes.len();
        // SAFETY: `MaybeUninit<u8>` and `u8` have the same layout, and the
        // destination is within `buf`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.buf[self.filled..end].as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        if self.initialized < end {
            self.initialized = end;
        }
        self.filled = end;
    }

    /// The unfilled part, as memory that may be uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must not write uninitialized bytes into it: the part known
    /// to be initialized must stay initialized.
    pub unsafe fn unfilled_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.buf[self.filled..]
    }

    fn unfilled_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.buf[self.filled..]
    }
}

impl fmt::Debug for ReadBuf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadBuf")
            .field("filled", &self.filled)
            .field("initialized", &self.initialized)
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// # Safety
///
/// Every byte of `slice` is initialized.
unsafe fn slice_assume_init(slice: &[MaybeUninit<u8>]) -> &[u8] {
    // SAFETY: same layout; initialized by the caller's promise.
    unsafe { &*(core::ptr::from_ref::<[MaybeUninit<u8>]>(slice) as *const [u8]) }
}

/// # Safety
///
/// Every byte of `slice` is initialized.
unsafe fn slice_assume_init_mut(slice: &mut [MaybeUninit<u8>]) -> &mut [u8] {
    // SAFETY: as for `slice_assume_init`.
    unsafe { &mut *(core::ptr::from_mut::<[MaybeUninit<u8>]>(slice) as *mut [u8]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_buffer_is_initialized_and_empty() {
        let mut bytes = [7u8; 8];
        let buf = ReadBuf::new(&mut bytes);
        assert_eq!(buf.capacity(), 8);
        assert_eq!(buf.remaining(), 8);
        assert!(buf.filled().is_empty());
        assert_eq!(buf.initialized(), &[7; 8]);
    }

    #[test]
    fn put_slice_and_advance_count_the_filled_part() {
        let mut bytes = [0u8; 8];
        let mut buf = ReadBuf::new(&mut bytes);
        buf.put_slice(b"abc");
        assert_eq!(buf.filled(), b"abc");
        buf.initialize_unfilled_to(2).copy_from_slice(b"de");
        buf.advance(2);
        assert_eq!(buf.filled(), b"abcde");
        assert_eq!(buf.remaining(), 3);
        buf.clear();
        assert!(buf.filled().is_empty());
        assert_eq!(
            buf.initialized().len(),
            8,
            "clearing keeps what was initialized"
        );
    }

    #[test]
    fn an_uninit_buffer_initializes_only_what_is_asked() {
        let mut mem = [MaybeUninit::<u8>::uninit(); 16];
        let mut buf = ReadBuf::uninit(&mut mem);
        assert!(buf.initialized().is_empty());
        let part = buf.initialize_unfilled_to(4);
        assert_eq!(part, &[0; 4]);
        part.copy_from_slice(b"wxyz");
        buf.advance(4);
        assert_eq!(buf.initialized().len(), 4);
        assert_eq!(buf.filled(), b"wxyz");
        let rest = buf.initialize_unfilled();
        assert_eq!(rest.len(), 12);
        assert_eq!(buf.initialized().len(), 16);
    }

    #[test]
    fn take_shares_the_initialized_part_and_bounds_the_room() {
        let mut bytes = [1u8; 10];
        let mut buf = ReadBuf::new(&mut bytes);
        buf.put_slice(&[9, 9]);
        let mut taken = buf.take(3);
        assert_eq!(taken.capacity(), 3);
        assert_eq!(taken.initialized().len(), 3);
        taken.put_slice(&[5, 5, 5]);
        assert_eq!(taken.filled(), &[5, 5, 5]);
    }

    #[test]
    #[should_panic(expected = "filled must not pass the initialized part")]
    fn advancing_past_the_initialized_part_panics() {
        let mut mem = [MaybeUninit::<u8>::uninit(); 4];
        let mut buf = ReadBuf::uninit(&mut mem);
        buf.advance(1);
    }

    #[test]
    #[should_panic(expected = "do not fit")]
    fn put_slice_past_the_room_panics() {
        let mut bytes = [0u8; 2];
        ReadBuf::new(&mut bytes).put_slice(b"abc");
    }
}
