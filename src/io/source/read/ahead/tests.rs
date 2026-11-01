//! The read-ahead keeps the bytes in the order they came, whether a piece sits first or
//! in the queue behind it, and lets go of every piece it empties.

use std::io;

use super::{Ahead, Piece};
use crate::io::side::After;

fn piece(bytes: &[u8]) -> Piece {
    Piece::Left {
        buf: bytes.to_vec(),
        start: 0,
    }
}

fn ahead(pieces: &[&[u8]]) -> Ahead {
    let mut ahead = Ahead::default();
    let mut after = After::default();
    for bytes in pieces {
        ahead.push(piece(bytes), &mut after);
    }
    after.run();
    ahead
}

/// Reads `len` bytes at most, as a stream read takes them.
fn read(ahead: &mut Ahead, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    let mut after = After::default();
    let n = ahead.copy_into(&mut out, false, &mut after, None);
    after.run();
    out.truncate(n);
    out
}

#[test]
fn bytes_come_out_in_order_across_the_first_piece_and_the_queue() {
    let mut ahead = ahead(&[b"abc", b"de", b"fgh"]);
    assert_eq!(ahead.len(), 8);
    assert_eq!(read(&mut ahead, 2), b"ab");
    assert_eq!(read(&mut ahead, 4), b"cdef");
    assert_eq!(ahead.len(), 2);
    assert_eq!(read(&mut ahead, 10), b"gh");
    assert!(ahead.is_empty());
    assert_eq!(read(&mut ahead, 4), b"");
}

#[test]
fn a_piece_that_comes_after_the_last_one_went_is_first() {
    let mut ahead = ahead(&[b"ab"]);
    assert_eq!(read(&mut ahead, 2), b"ab");
    assert!(ahead.is_empty());
    let mut after = After::default();
    ahead.push(piece(b"cd"), &mut after);
    ahead.push(piece(b"ef"), &mut after);
    after.run();
    assert_eq!(read(&mut ahead, 3), b"cde");
    assert_eq!(read(&mut ahead, 3), b"f");
}

#[test]
fn a_peek_leaves_every_byte_for_the_read() {
    let mut ahead = ahead(&[b"ab", b"cd"]);
    let mut out = [0; 3];
    let mut after = After::default();
    assert_eq!(ahead.copy_into(&mut out, true, &mut after, None), 3);
    after.run();
    assert_eq!(&out, b"abc");
    assert_eq!(ahead.len(), 4);
    assert_eq!(read(&mut ahead, 4), b"abcd");
}

#[test]
fn a_read_the_first_piece_covers_takes_from_it_alone() {
    let mut ahead = ahead(&[b"abcdef", b"gh"]);
    let mut out = [0; 2];
    let mut after = After::default();
    assert_eq!(ahead.copy_into(&mut out, true, &mut after, None), 2);
    after.run();
    assert_eq!(&out, b"ab");
    assert_eq!(read(&mut ahead, 2), b"ab");
    assert_eq!(read(&mut ahead, 3), b"cde");
    assert_eq!(ahead.len(), 3);
    assert_eq!(read(&mut ahead, 1), b"f");
    assert_eq!(ahead.len(), 2);
    assert_eq!(read(&mut ahead, 3), b"gh");
    assert!(ahead.is_empty());
}

#[test]
fn an_empty_piece_is_let_go_at_once() {
    let mut ahead = ahead(&[b""]);
    assert!(ahead.is_empty());
    let mut after = After::default();
    ahead.push(piece(b"x"), &mut after);
    ahead.push(piece(b""), &mut after);
    after.run();
    assert_eq!(ahead.len(), 1);
    assert_eq!(read(&mut ahead, 4), b"x");
    assert!(ahead.is_empty());
}

#[test]
fn vectored_reads_and_appends_take_the_bytes_in_order() {
    let mut ahead = ahead(&[b"abc", b"def"]);
    let (mut a, mut b) = ([0; 2], [0; 3]);
    let mut after = After::default();
    let n = ahead.copy_vectored(
        &mut [io::IoSliceMut::new(&mut a), io::IoSliceMut::new(&mut b)],
        &mut after,
        None,
    );
    assert_eq!((n, &a, &b), (5, b"ab", b"cde"));
    let mut buf = crate::buf::owned::OwnedBuf::Vec(Vec::with_capacity(4));
    assert_eq!(ahead.append_to(&mut buf, &mut after, None), 1);
    after.run();
    assert!(ahead.is_empty());
    assert_eq!(buf.get(..), Some(&b"f"[..]));
}

#[test]
fn a_release_lets_go_of_every_piece() {
    let mut ahead = ahead(&[b"ab", b"cd", b"ef"]);
    let mut after = After::default();
    ahead.release(&mut after);
    after.run();
    assert!(ahead.is_empty());
    assert_eq!(ahead.len(), 0);
    assert_eq!(read(&mut ahead, 4), b"");
}
