use crate::buf::aligned::AlignedBuf;
use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};
use crate::buf::vectored;

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
#[test]
fn vectored_buffers_come_back_in_order_and_unmoved() {
    let bufs = vec![b"one".to_vec(), b"two".to_vec(), Vec::new()];
    let ptrs: Vec<_> = bufs.iter().map(|b| b.as_ptr()).collect();
    let owned = vectored::written(bufs);
    assert_eq!(owned.len(), 3);
    assert!(owned.iter().all(|b| matches!(b, OwnedBuf::Vec(_))));
    let back: Vec<Vec<u8>> = vectored::into_written(owned);
    assert_eq!(back, [b"one".to_vec(), b"two".to_vec(), Vec::new()]);
    assert_eq!(back.iter().map(|b| b.as_ptr()).collect::<Vec<_>>(), ptrs);

    let arrays = [
        AlignedBuf::new(512, 512).unwrap(),
        AlignedBuf::new(1024, 512).unwrap(),
    ];
    let ptrs = arrays.each_ref().map(|b| b.as_ptr());
    let back: [AlignedBuf; 2] = vectored::into_filled(vectored::filled(arrays));
    assert_eq!(back.each_ref().map(|b| b.as_ptr()), ptrs);
    assert_eq!(back.each_ref().map(|b| b.capacity()), [512, 1024]);
}

#[test]
fn an_array_missing_a_buffer_gets_an_empty_one_in_its_place() {
    let back: [Mine; 3] = vectored::into_filled(vec![OwnedBuf::filled(Mine::new(2, b"x"))]);
    assert_eq!(back[0], Mine::new(2, b"x"));
    assert_eq!(back[1], Mine::default());
    assert_eq!(back[2], Mine::default());
}

#[test]
fn a_vectored_write_reads_as_its_buffers_in_order() {
    let segs = vec![
        OwnedBuf::written(b"abc".to_vec()),
        OwnedBuf::written(Vec::new()),
        OwnedBuf::written(Fixed(b"defg")),
        OwnedBuf::written(b"h".to_vec()),
    ];
    let mut owned = OwnedBuf::vectored(segs);
    assert_eq!((owned.len(), owned.capacity()), (8, 8));
    assert_eq!(&owned[..], b"", "no one slice of bytes");
    let rests: Vec<&[u8]> = (0..=9).map(|at| owned.rest_from(at)).collect();
    assert_eq!(
        rests,
        [
            &b"abc"[..],
            b"bc",
            b"c",
            b"defg",
            b"efg",
            b"fg",
            b"g",
            b"h",
            b"",
            b""
        ]
    );
    assert!(owned.spare_capacity_mut().is_empty());
    assert_eq!(owned.append(b"x"), 0);
    owned.truncate(2);
    assert_eq!(owned.len(), 8, "a write's bytes are never cut");
    let back = owned.into_segments();
    assert_eq!(back.len(), 4);
    assert_eq!(&back[2][..], b"defg");
}

#[test]
fn a_single_buffer_is_its_own_segment_and_rest() {
    let owned = OwnedBuf::written(b"single".to_vec());
    assert_eq!(owned.rest_from(2), b"ngle");
    assert_eq!(owned.rest_from(6), b"");
    assert_eq!(owned.rest_from(60), b"");
    let segs = owned.into_segments();
    assert_eq!(segs.len(), 1);
    assert_eq!(&segs[0][..], b"single");
}

#[test]
fn the_runtimes_own_form_passes_through_unchanged() {
    let vec = b"through".to_vec();
    let ptr = vec.as_ptr();
    let owned = OwnedBuf::filled(OwnedBuf::Vec(vec));
    assert!(matches!(owned, OwnedBuf::Vec(_)));
    let again: OwnedBuf = owned.into_filled();
    assert_eq!(again.as_ptr(), ptr);
    let vectored = OwnedBuf::written(OwnedBuf::vectored(vec![again]));
    assert!(matches!(vectored, OwnedBuf::Vectored(_)));
    let back: Vec<u8> = vectored.into_written();
    assert!(back.is_empty(), "a vectored write is no one buffer");
}
