use crate::buf::aligned::AlignedBuf;
use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};

/// A caller's own buffer: boxed memory and a count of the bytes it holds.
#[derive(Debug, Default, PartialEq)]
struct Mine {
    mem: Box<[u8]>,
    len: usize,
}

impl Mine {
    fn new(cap: usize, held: &[u8]) -> Self {
        let mut mem = vec![0u8; cap].into_boxed_slice();
        mem[..held.len()].copy_from_slice(held);
        Self {
            mem,
            len: held.len(),
        }
    }
}

// SAFETY: the boxed memory does not move with the value.
unsafe impl IoBuf for Mine {
    fn stable_ptr(&self) -> *const u8 {
        self.mem.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len
    }

    fn bytes_total(&self) -> usize {
        self.mem.len()
    }
}

// SAFETY: as above; all of it is writable.
unsafe impl IoBufMut for Mine {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.mem.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        self.len = pos;
    }
}

/// A write-only buffer of static bytes.
#[derive(Default)]
struct Fixed(&'static [u8]);

// SAFETY: static bytes never move.
unsafe impl IoBuf for Fixed {
    fn stable_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.0.len()
    }

    fn bytes_total(&self) -> usize {
        self.0.len()
    }
}

#[test]
fn a_vec_goes_through_as_it_is() {
    let mut vec = Vec::with_capacity(100);
    vec.extend_from_slice(b"ten bytes!");
    let (ptr, cap) = (vec.as_ptr(), vec.capacity());
    let owned = OwnedBuf::filled(vec);
    assert!(matches!(owned, OwnedBuf::Vec(_)));
    assert_eq!(
        (owned.len(), owned.capacity(), owned.as_ptr()),
        (10, cap, ptr)
    );
    let back: Vec<u8> = owned.into_filled();
    assert_eq!(
        (back.as_ptr(), back.capacity(), &back[..]),
        (ptr, cap, &b"ten bytes!"[..])
    );
    let back: Vec<u8> = OwnedBuf::written(back).into_written();
    assert_eq!(back.as_ptr(), ptr);
}

#[test]
fn an_aligned_buffer_goes_through_as_it_is() {
    let mut buf = AlignedBuf::new(4096, 4096).unwrap();
    buf.extend_from_slice(b"aligned");
    let ptr = buf.as_ptr();
    let owned = OwnedBuf::filled(buf);
    assert!(matches!(owned, OwnedBuf::Aligned(_)));
    let back: AlignedBuf = owned.into_filled();
    assert_eq!(
        (back.as_ptr(), &back[..], back.alignment()),
        (ptr, &b"aligned"[..], 4096)
    );
    let back: AlignedBuf = OwnedBuf::written(back).into_written();
    assert_eq!(back.as_ptr(), ptr);
}

#[test]
fn a_callers_type_reads_into_its_spare_room_and_comes_back_with_its_count() {
    let mine = Mine::new(8, b"ab");
    let ptr = mine.mem.as_ptr();
    let mut owned = OwnedBuf::filled(mine);
    assert!(matches!(owned, OwnedBuf::Other(_)));
    assert_eq!(
        (owned.len(), owned.capacity(), &owned[..]),
        (2, 8, &b"ab"[..])
    );
    assert_eq!(owned.spare_capacity_mut().len(), 6);
    assert_eq!(owned.append(b"cdefghij"), 6);
    owned.truncate(5);
    let back: Mine = owned.into_filled();
    assert_eq!(back.mem.as_ptr(), ptr);
    assert_eq!(back.len, 5);
    assert_eq!(&back.mem[..5], b"abcde");
}

#[test]
fn a_write_never_reads_into_a_callers_buffer() {
    let mut owned = OwnedBuf::written(Fixed(b"static bytes"));
    assert_eq!((owned.len(), owned.capacity()), (12, 12));
    assert!(owned.spare_capacity_mut().is_empty());
    assert!(owned.spare_zeroed().is_empty());
    assert_eq!(owned.append(b"more"), 0);
    assert_eq!(&owned[..], b"static bytes");
    let back: Fixed = owned.into_written();
    assert_eq!(back.0, b"static bytes");
}

#[test]
fn a_buffer_that_cannot_come_back_is_answered_with_an_empty_one() {
    let mine: Mine = OwnedBuf::default().into_filled();
    assert_eq!(mine, Mine::default());
    let aligned: AlignedBuf = OwnedBuf::Vec(vec![1, 2, 3]).into_filled();
    assert_eq!((aligned.len(), aligned.capacity()), (0, 0));
    let vec: Vec<u8> = OwnedBuf::written(Fixed(b"x")).into_written();
    assert!(vec.is_empty());
    let mine: Mine = OwnedBuf::written(Mine::new(4, b"w")).into_filled();
    assert_eq!(mine, Mine::default(), "a write's box is not a read's");
}

#[test]
fn append_cuts_at_the_spare_capacity_and_truncate_only_shortens() {
    let mut owned = OwnedBuf::filled(Vec::with_capacity(4));
    assert_eq!(owned.append(b"a"), 1);
    assert_eq!(owned.append(b"bcdef"), 3);
    assert_eq!(&owned[..], b"abcd");
    owned.truncate(9);
    assert_eq!(owned.len(), 4);
    owned.truncate(2);
    assert_eq!(&owned[..], b"ab");
}

#[test]
fn spare_zeroed_zeroes_the_room_and_keeps_the_length() {
    let mut vec = Vec::with_capacity(8);
    vec.extend_from_slice(b"hi");
    let mut owned = OwnedBuf::filled(vec);
    let room = owned.spare_zeroed();
    assert_eq!(room.len(), 6);
    room[0] = b'!';
    assert_eq!(owned.len(), 2);
    // SAFETY: the room was zeroed, and one byte written.
    unsafe { owned.set_len(4) };
    assert_eq!(&owned[..], b"hi!\0");
}

#[test]
fn an_empty_callers_buffer_reads_as_no_bytes() {
    let owned = OwnedBuf::filled(Mine::default());
    assert_eq!(&owned[..], b"");
    assert_eq!(owned.capacity(), 0);
}
