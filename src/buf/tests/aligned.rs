use std::io;

use crate::buf::aligned::AlignedBuf;
use crate::buf::owned::{IoBuf, IoBufMut};

const fn is_send_sync<T>()
where
    T: Send + Sync,
{
}
const _: () = is_send_sync::<AlignedBuf>();

#[test]
fn its_memory_starts_at_every_power_of_two_it_is_asked_for() {
    for shift in 0..=16 {
        let align = 1usize << shift;
        for capacity in [1, 511, 512, 4095, 4096, 4097, 65536] {
            let buf = AlignedBuf::new(capacity, align).unwrap();
            assert_eq!(
                buf.as_ptr() as usize % align,
                0,
                "align {align} capacity {capacity}"
            );
            assert_eq!(
                (buf.len(), buf.capacity(), buf.alignment()),
                (0, capacity, align)
            );
            assert!(buf.is_empty());
        }
    }
}

#[test]
fn an_empty_one_has_no_memory_and_an_aligned_address() {
    for align in [1, 512, 4096] {
        let mut buf = AlignedBuf::new(0, align).unwrap();
        assert_eq!(buf.as_ptr() as usize % align, 0);
        assert_eq!(buf.capacity(), 0);
        assert!(buf.spare_capacity_mut().is_empty());
        assert_eq!(buf.extend_from_slice(b"x"), 0);
        assert_eq!(&*buf, b"");
    }
    let default = AlignedBuf::default();
    assert_eq!((default.len(), default.capacity()), (0, 0));
}

#[test]
fn an_alignment_that_is_not_a_power_of_two_is_refused() {
    for align in [0, 3, 6, 4097, usize::MAX] {
        let err = AlignedBuf::new(16, align).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "align {align}");
        let err = AlignedBuf::zeroed(16, align).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "align {align}");
    }
}

#[test]
fn a_capacity_past_what_memory_can_hold_is_refused() {
    for (capacity, align) in [(usize::MAX, 1), (isize::MAX as usize, 4096)] {
        let err = AlignedBuf::new(capacity, align).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn zeroed_holds_its_whole_length_in_zeroes() {
    for len in [0, 1, 4096, 4097] {
        let buf = AlignedBuf::zeroed(len, 4096).unwrap();
        assert_eq!((buf.len(), buf.capacity()), (len, len));
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(buf.as_ptr() as usize % 4096, 0);
    }
}

#[test]
fn extend_from_slice_stops_at_its_capacity() {
    let mut buf = AlignedBuf::new(8, 8).unwrap();
    assert_eq!(buf.extend_from_slice(b"abc"), 3);
    assert_eq!(buf.extend_from_slice(b"defghijk"), 5);
    assert_eq!(buf.extend_from_slice(b"z"), 0);
    assert_eq!(&*buf, b"abcdefgh");
    assert!(buf.spare_capacity_mut().is_empty());
}

#[test]
fn clear_truncate_and_set_len_move_only_the_length() {
    let mut buf = AlignedBuf::zeroed(16, 16).unwrap();
    buf[..4].copy_from_slice(b"wxyz");
    buf.truncate(32);
    assert_eq!(buf.len(), 16);
    buf.truncate(4);
    assert_eq!(&*buf, b"wxyz");
    assert_eq!(buf.spare_capacity_mut().len(), 12);
    // SAFETY: the 16 bytes were zeroed when it was made.
    unsafe { buf.set_len(16) };
    assert_eq!(&buf[..6], b"wxyz\0\0");
    let ptr = buf.as_ptr();
    buf.clear();
    assert!(buf.is_empty());
    assert_eq!((buf.capacity(), buf.as_ptr()), (16, ptr));
}

#[test]
fn it_is_an_owned_buffer_of_its_own_memory() {
    let mut buf = AlignedBuf::new(4096, 4096).unwrap();
    buf.extend_from_slice(b"held");
    assert_eq!(buf.stable_ptr(), buf.as_ptr());
    assert_eq!((buf.bytes_init(), buf.bytes_total()), (4, 4096));
    let ptr = buf.stable_mut_ptr();
    // SAFETY: the buffer's own memory, past its four bytes.
    unsafe {
        ptr.add(4).copy_from_nonoverlapping(b"more".as_ptr(), 4);
        buf.set_init(8);
    }
    assert_eq!(&*buf, b"heldmore");
}

#[test]
fn its_debug_shows_its_sizes() {
    let buf = AlignedBuf::new(64, 32).unwrap();
    assert_eq!(
        format!("{buf:?}"),
        "AlignedBuf { len: 0, capacity: 64, alignment: 32 }"
    );
}
