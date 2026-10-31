//! A stream's read-ahead: the bytes the driver read that no task took yet,
//! in the order they came, as pieces of the pool's buffers, of lent buffers
//! whose task went away, on a ring of the receive group's buffers, and on a
//! port of the read slot's own room.

use std::collections::VecDeque;
use std::io;

use crate::buf::PoolBuf;
use crate::io::side::After;
use crate::rt::shard::Shard;

/// One run of bytes read ahead.
pub(super) enum Piece {
    /// Read by the driver into a buffer of the pool.
    Pool(PoolBuf),
    /// Read into a lent buffer, from `start`, whose task went away.
    // vertexia: the caller's allocation, kept uncharged by the pool (one per
    // source at most, freed by the next reads); copy it into pool buffers if
    // kept buffers must count against the cap.
    Left { buf: Vec<u8>, start: usize },
    /// A buffer of the shard's receive group a multishot receive filled;
    /// it goes back to the group once taken.
    #[cfg(ring)]
    Ring(crate::rt::shard::recv::RingChunk),
    /// On a port, read into the read slot's own room, for a read the pool had
    /// no buffer for.
    #[cfg(port)]
    Room(crate::io::source::ops::Room),
}

impl Piece {
    fn filled(&self) -> &[u8] {
        match self {
            Piece::Pool(buf) => buf.filled(),
            Piece::Left { buf, start } => buf.get(*start..).unwrap_or_default(),
            #[cfg(ring)]
            Piece::Ring(chunk) => chunk.bytes(),
            #[cfg(port)]
            Piece::Room(room) => room.bytes(),
        }
    }

    fn consume(&mut self, n: usize) {
        match self {
            Piece::Pool(buf) => buf.consume(n),
            Piece::Left { buf, start } => *start = (*start + n).min(buf.len()),
            #[cfg(ring)]
            Piece::Ring(chunk) => chunk.consume(n),
            #[cfg(port)]
            Piece::Room(room) => room.consume(n),
        }
    }

    /// Lets go of the piece; a chunk of the receive group goes back through
    /// `shard`, the shard the caller runs, if it knows it.
    #[inline(always)]
    fn release(self, after: &mut After, shard: Option<&Shard>) {
        match self {
            Piece::Pool(buf) => after.drop_buf(buf),
            #[cfg(ring)]
            Piece::Ring(chunk) => chunk.give_back(shard),
            _ => {}
        }
        #[cfg(not(ring))]
        let _ = shard;
    }
}

/// Bytes read ahead that no task took yet, in the order they came: what
/// the driver read into the pool, what a lent buffer whose task went away
/// holds, what multishot receives handed out.
#[derive(Default)]
pub(super) struct Ahead {
    pieces: VecDeque<Piece>,
}

impl Ahead {
    pub(super) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The bytes held.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.pieces.iter().map(|p| p.filled().len()).sum()
    }

    /// Adds bytes that came after those held.
    #[inline(always)]
    pub(super) fn push(&mut self, piece: Piece, after: &mut After) {
        if piece.filled().is_empty() {
            piece.release(after, None);
        } else {
            self.pieces.push_back(piece);
        }
    }

    /// Takes `n` bytes off the front, letting go of the pieces emptied.
    pub(super) fn consume(&mut self, mut n: usize, after: &mut After, shard: Option<&Shard>) {
        while n > 0 {
            let Some(front) = self.pieces.front_mut() else {
                return;
            };
            let here = front.filled().len().min(n);
            front.consume(here);
            n -= here;
            if front.filled().is_empty()
                && let Some(done) = self.pieces.pop_front()
            {
                done.release(after, shard);
            }
        }
    }

    /// Copies bytes into `out`, in order, consumed unless `peek`; returns
    /// how many.
    #[inline]
    pub(super) fn copy_into(
        &mut self,
        out: &mut [u8],
        peek: bool,
        after: &mut After,
        shard: Option<&Shard>,
    ) -> usize {
        // One piece, the usual case: one look at its bytes.
        if self.pieces.len() == 1
            && let Some(piece) = self.pieces.front_mut()
        {
            let src = piece.filled();
            let (all, n) = (src.len(), src.len().min(out.len()));
            out[..n].copy_from_slice(&src[..n]);
            if !peek {
                if n == all {
                    if let Some(done) = self.pieces.pop_front() {
                        done.release(after, shard);
                    }
                } else {
                    piece.consume(n);
                }
            }
            return n;
        }
        self.copy_pieces(out, peek, after, shard)
    }

    /// [`copy_into`](Self::copy_into) from any number of pieces.
    #[inline(never)]
    fn copy_pieces(
        &mut self,
        out: &mut [u8],
        peek: bool,
        after: &mut After,
        shard: Option<&Shard>,
    ) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let src = piece.filled();
            let take = src.len().min(out.len() - n);
            out[n..n + take].copy_from_slice(&src[..take]);
            n += take;
            if n == out.len() {
                break;
            }
        }
        if !peek {
            self.consume(n, after, shard);
        }
        n
    }

    /// Copies bytes into `bufs`, in order, and consumes them.
    pub(super) fn copy_vectored(
        &mut self,
        bufs: &mut [io::IoSliceMut<'_>],
        after: &mut After,
        shard: Option<&Shard>,
    ) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let copied = copy_vectored_from(piece.filled(), bufs, n);
            n += copied;
            if copied < piece.filled().len() {
                break;
            }
        }
        self.consume(n, after, shard);
        n
    }

    /// Moves bytes onto the end of `buf`, as many as its spare capacity
    /// holds.
    pub(super) fn append_to(
        &mut self,
        buf: &mut Vec<u8>,
        after: &mut After,
        shard: Option<&Shard>,
    ) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let src = piece.filled();
            let copied = crate::buf::append_spare(src, buf);
            n += copied;
            if copied < src.len() {
                break;
            }
        }
        self.consume(n, after, shard);
        n
    }

    /// Lets go of every byte held.
    pub(super) fn release(&mut self, after: &mut After) {
        for piece in self.pieces.drain(..) {
            piece.release(after, None);
        }
    }
}

/// Copies `src` into `bufs` after the first `skip` bytes they hold;
/// returns how many bytes it copied.
fn copy_vectored_from(src: &[u8], bufs: &mut [io::IoSliceMut<'_>], mut skip: usize) -> usize {
    let mut n = 0;
    for buf in bufs.iter_mut() {
        if skip >= buf.len() {
            skip -= buf.len();
            continue;
        }
        let room = &mut buf[skip..];
        skip = 0;
        let take = room.len().min(src.len() - n);
        room[..take].copy_from_slice(&src[n..n + take]);
        n += take;
        if n == src.len() {
            break;
        }
    }
    n
}
