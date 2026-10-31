use proptest::prelude::*;

use crate::buf::owned::OwnedBuf;
use crate::buf::vectored::{IOV_MAX, IoVec, Vectored};

fn held(bytes: &[u8], cap: usize) -> OwnedBuf {
    let mut vec = Vec::with_capacity(cap);
    vec.extend_from_slice(bytes);
    OwnedBuf::Vec(vec)
}

fn parts(iov: &IoVec) -> (usize, usize) {
    #[cfg(unix)]
    return (iov.iov_base as usize, iov.iov_len);
    #[cfg(windows)]
    return (iov.buf as usize, iov.len as usize);
}

/// The kernel's part of a vectored read: `bytes` written into the rooms the
/// descriptors name, in order.
fn kernel_fills(iov: &[IoVec], bytes: &[u8]) -> usize {
    let mut n = 0;
    for room in iov {
        let (ptr, len) = parts(room);
        let take = len.min(bytes.len() - n);
        // SAFETY: the descriptor names `len` writable bytes of a buffer the
        // test owns.
        unsafe { core::ptr::copy_nonoverlapping(bytes[n..].as_ptr(), ptr as *mut u8, take) };
        n += take;
    }
    n
}

#[test]
fn a_read_fills_each_room_in_order_after_the_bytes_held() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"ab", 4), held(b"", 3), held(b"z", 4)]);
    assert_eq!((owned.len(), owned.capacity()), (3, 11));
    let bufs = owned.vectored_mut().unwrap();
    let rooms: Vec<usize> = bufs.read_iovecs().iter().map(|i| parts(i).1).collect();
    assert_eq!(rooms, [2, 3, 3]);
    let n = kernel_fills(bufs.read_iovecs(), b"1234567");
    assert_eq!(n, 7);
    // SAFETY: the 7 bytes were written into the rooms in order.
    unsafe { owned.set_len(owned.len() + n) };
    assert_eq!(owned.len(), 10);
    let back = owned.into_segments();
    assert_eq!(&back[0][..], b"ab12");
    assert_eq!(&back[1][..], b"345");
    assert_eq!(&back[2][..], b"z67");
}

#[test]
fn a_short_read_leaves_the_later_rooms_as_they_were() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"", 2), held(b"x", 3), held(b"", 2)]);
    let n = kernel_fills(owned.vectored_mut().unwrap().read_iovecs(), b"abc");
    // SAFETY: the 3 bytes were written into the rooms in order.
    unsafe { owned.set_len(owned.len() + n) };
    let back = owned.into_segments();
    assert_eq!(
        (&back[0][..], &back[1][..], &back[2][..]),
        (&b"ab"[..], &b"xc"[..], &b""[..])
    );
}

#[test]
fn a_read_cut_back_returns_every_buffer_to_what_it_held() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"a", 2), held(b"b", 2)]);
    let n = kernel_fills(owned.vectored_mut().unwrap().read_iovecs(), b"12");
    // SAFETY: as above.
    unsafe { owned.set_len(owned.len() + n) };
    assert_eq!(owned.len(), 4);
    owned.truncate(2);
    let back = owned.into_segments();
    assert_eq!((&back[0][..], &back[1][..]), (&b"a"[..], &b"b"[..]));
}

#[test]
fn appended_bytes_fill_the_rooms_in_order() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"h", 2), held(b"", 0), held(b"", 3)]);
    assert_eq!(owned.append(b"abcdef"), 4);
    assert_eq!(owned.append(b"z"), 0);
    let back = owned.into_segments();
    assert_eq!((&back[0][..], &back[2][..]), (&b"ha"[..], &b"bcd"[..]));
}

#[test]
fn full_buffers_give_no_room_to_read_into() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"ab", 2), OwnedBuf::default()]);
    assert_eq!(owned.len(), owned.capacity());
    assert!(owned.vectored_mut().unwrap().read_iovecs().is_empty());
}

#[test]
fn a_reads_buffers_left_by_their_task_keep_only_the_bytes_read() {
    let mut owned = OwnedBuf::vectored_read(vec![held(b"old", 4), held(b"", 4), held(b"", 4)]);
    let n = kernel_fills(owned.vectored_mut().unwrap().read_iovecs(), b"12345");
    // SAFETY: as above.
    unsafe { owned.set_len(owned.len() + n) };
    let parts: Vec<(Vec<u8>, usize)> = owned
        .into_vectored()
        .unwrap()
        .read_parts()
        .map(|(buf, start)| (buf.to_vec(), start))
        .collect();
    assert_eq!(parts, [(b"old1".to_vec(), 3), (b"2345".to_vec(), 0)]);
}

#[test]
fn a_writes_descriptors_start_at_the_byte_asked_and_skip_empty_buffers() {
    let mut bufs = Vectored::written(vec![held(b"abc", 3), held(b"", 0), held(b"de", 2)]);
    let at = |bufs: &mut Vectored, from| -> Vec<usize> {
        bufs.write_iovecs(from).iter().map(|i| parts(i).1).collect()
    };
    assert_eq!(at(&mut bufs, 0), [3, 2]);
    assert_eq!(at(&mut bufs, 1), [2, 2]);
    assert_eq!(at(&mut bufs, 3), [2]);
    assert_eq!(at(&mut bufs, 4), [1]);
    assert!(at(&mut bufs, 5).is_empty());
    assert!(at(&mut bufs, 50).is_empty());
}

#[test]
fn one_call_names_at_most_iov_max_buffers() {
    let segs = (0..IOV_MAX + 7).map(|_| held(b"x", 1)).collect();
    let mut bufs = Vectored::written(segs);
    assert_eq!(bufs.write_iovecs(0).len(), IOV_MAX);
    assert_eq!(bufs.write_iovecs(IOV_MAX).len(), 7);
    let mut rooms = Vectored::filled((0..IOV_MAX + 3).map(|_| held(b"", 1)).collect());
    assert_eq!(rooms.read_iovecs().len(), IOV_MAX);
}

proptest! {
    /// However the buffers are sized and however many bytes a read brings,
    /// each buffer keeps what it held, gains at most its room, and a buffer
    /// gains bytes only once every buffer before it is full.
    #[test]
    fn a_read_fills_rooms_in_order(
        shapes in proptest::collection::vec((0usize..6, 0usize..6), 0..8),
        extra in 0usize..40,
    ) {
        let segs: Vec<OwnedBuf> = shapes
            .iter()
            .map(|&(held_len, room)| held(&vec![b'h'; held_len], held_len + room))
            .collect();
        let mut owned = OwnedBuf::vectored_read(segs);
        let room: usize = shapes.iter().map(|&(_, r)| r).sum();
        let bytes: Vec<u8> = (0..extra.min(room)).map(|i| i as u8).collect();
        let n = kernel_fills(owned.vectored_mut().unwrap().read_iovecs(), &bytes);
        prop_assert_eq!(n, bytes.len());
        // SAFETY: the bytes were written into the rooms in order.
        unsafe { owned.set_len(owned.len() + n) };
        let back = owned.into_segments();
        let mut seen = 0;
        let mut short = false;
        for (buf, &(held_len, room)) in back.iter().zip(&shapes) {
            let gained = buf.len() - held_len;
            prop_assert!(gained <= room);
            prop_assert!(buf[..held_len].iter().all(|&b| b == b'h'));
            prop_assert_eq!(&buf[held_len..], &bytes[seen..seen + gained]);
            prop_assert!(!(short && gained > 0), "a later buffer gained before an earlier one was full");
            short |= gained < room;
            seen += gained;
        }
        prop_assert_eq!(seen, bytes.len());
    }
}
