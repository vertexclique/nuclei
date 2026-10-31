//! Many positional reads in one call: [`Handle::read_many_at`]. The reads
//! wait once for the cursor writes accepted before the call (as each
//! positional read does), then go out together: on io_uring every read the
//! window holds is an entry of the shard's ring, handed to the kernel in the
//! shard's one submission; elsewhere each is a job of the blocking pool. At
//! most [`MANY_AT_ONCE`] are out at a time, the next going out as one ends,
//! so a call of any length holds a bounded share of the ring and the pool.
//! The reads read into the caller's own buffers, which the runtime's buffer
//! cap does not count. `proofs/tla/many/ReadMany.tla` checks the window.

use core::future::poll_fn;
use core::task::Poll;
use std::fs::File as StdFile;
use std::io;
use std::sync::Arc;

use super::FileIo;
use super::work::Work;
use crate::buf::owned::{IoBufMut, OwnedBuf};
use crate::io::Handle;
use crate::io::source::Failure;

/// The most reads of one call out at once.
pub(crate) const MANY_AT_ONCE: usize = 64;

/// A read's end: its result, and the caller's buffer with what it read.
type End = (io::Result<usize>, OwnedBuf);

impl FileIo {
    pub(crate) async fn read_many_at<B>(
        self: &Arc<Self>,
        reads: Vec<(B, u64)>,
    ) -> Vec<(io::Result<usize>, B)>
    where
        B: IoBufMut,
    {
        poll_fn(|cx| self.poll_ordered(cx)).await;
        let mut ends: Vec<Option<End>> = (0..reads.len()).map(|_| None).collect();
        let mut waiting = reads.into_iter().enumerate();
        let mut out: Vec<(usize, Work<End>)> = Vec::with_capacity(MANY_AT_ONCE.min(ends.len()));
        poll_fn(|cx| {
            loop {
                while out.len() < MANY_AT_ONCE {
                    let Some((i, (buf, at))) = waiting.next() else {
                        break;
                    };
                    let buf = OwnedBuf::filled(buf);
                    if buf.len() == buf.capacity() {
                        ends[i] = Some((Ok(0), buf));
                        continue;
                    }
                    out.push((i, self.submit_read_owned(buf, at)));
                }
                let before = out.len();
                out.retain_mut(|(i, work)| match work.poll(cx.waker()) {
                    Poll::Ready(end) => {
                        ends[*i] = Some(end);
                        false
                    }
                    Poll::Pending => true,
                });
                if out.is_empty() && waiting.len() == 0 {
                    return Poll::Ready(());
                }
                if out.len() == before {
                    return Poll::Pending;
                }
            }
        })
        .await;
        ends.into_iter()
            .map(|end| {
                let (result, buf) = end.unwrap_or_else(|| {
                    (
                        Err(io::Error::other("the read lost its buffer")),
                        OwnedBuf::default(),
                    )
                });
                (result, buf.into_filled())
            })
            .collect()
    }
}

impl Handle<StdFile> {
    /// Reads at each offset of `reads` into the spare capacity of its
    /// buffer, after the bytes it holds, and gives every buffer back in the
    /// order of `reads` with its read's result, as
    /// [`read_at_owned`](Self::read_at_owned) answers one: the bytes that
    /// came, zero at the end of the file or for a buffer with no spare
    /// capacity. The reads are made at once, in any order: on io_uring
    /// together, in the shard ring's one submission, at most 64 out at a
    /// time; elsewhere as jobs of the blocking pool, as many at a time.
    ///
    /// The reads see every write accepted at the cursor before the call, and
    /// every positional write that returned before it. Each read is one call
    /// of the system's positional read into its buffer itself, never copied.
    /// If the future is dropped, the reads still out are left to end by
    /// themselves, and their buffers go with them.
    ///
    /// # Errors
    ///
    /// Each read's own, as `read_at_owned`'s. On a stream (a pipe, a
    /// terminal) the file has no offset to read at, and every read answers
    /// the system's error for that.
    pub async fn read_many_at<B>(&self, reads: Vec<(B, u64)>) -> Vec<(io::Result<usize>, B)>
    where
        B: IoBufMut,
    {
        match self.file() {
            Ok(file) => file.read_many_at(reads).await,
            Err(err) => {
                let failure = Failure::of(&err);
                reads
                    .into_iter()
                    .map(|(buf, _)| (Err(failure.error()), buf))
                    .collect()
            }
        }
    }
}
