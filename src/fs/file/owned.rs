//! A handle's owned calls at the cursor: a read into the caller's buffer and
//! a write from it, each giving the buffer back with its result. The kernel
//! reads into or writes from the caller's memory itself; only bytes read
//! ahead are copied in.

use core::future::poll_fn;
use core::task::{Context, Poll};
use std::io;
use std::sync::Arc;

use super::work::{LentRead, tally_read};
use super::{FileHooks, FileIo, deferred};
use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};
use crate::io::side::After;

impl FileIo {
    /// Reads at the cursor into the spare capacity of `buf`, after the bytes
    /// it holds, and gives it back with the read's result. The job reads
    /// into `buf` itself (bytes read ahead are copied in instead).
    pub(crate) async fn read_owned<B>(self: &Arc<Self>, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBufMut,
    {
        let mut held = Some(OwnedBuf::filled(buf));
        // Dropped with the future, a job in flight is cancelled, or its
        // result thrown away: the cursor moves only when a result is taken.
        let mut job = None;
        let result = poll_fn(|cx| self.poll_read_owned(cx, &mut held, &mut job)).await;
        // Its read ended, so no job is left: the empty one is not dropped,
        // and its drop's call is not made on every read.
        if let Some(left) = job.take() {
            drop_cold(left);
        }
        core::mem::forget(job);
        (result, held.unwrap_or_default().into_filled())
    }

    fn poll_read_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<OwnedBuf>,
        job: &mut Option<LentRead>,
    ) -> Poll<io::Result<usize>> {
        if held.as_ref().is_some_and(|b| b.len() == b.capacity()) {
            return Poll::Ready(Ok(0));
        }
        self.with_state(cx, |st, io, waker, after| {
            loop {
                if let Some(lent) = job.as_mut() {
                    let pooled = lent.task.pooled();
                    let (result, mut buf) = match lent.task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(out) => out,
                    };
                    tally_read(pooled, &result, |n| *n);
                    let (at, epoch, generation, from) =
                        (lent.at, lent.epoch, lent.generation, lent.from);
                    *job = None;
                    if !st.counts(io, at, epoch, generation) {
                        // Another read, a write, a seek or a positional
                        // write's end moved on meanwhile: read again where
                        // the cursor is.
                        buf.truncate(from);
                        *held = Some(buf);
                        continue;
                    }
                    *held = Some(buf);
                    if let Ok(n) = result {
                        st.cursor += n as u64;
                        // Bytes read ahead at the old cursor.
                        if let Some(ahead) = st.ahead.take() {
                            after.drop_buf(ahead);
                        }
                    }
                    return Poll::Ready(result);
                }
                // A read sees every byte written before it.
                if st.poll_writes(io, waker, after).is_pending() {
                    return Poll::Pending;
                }
                let Some(mut buf) = held.take() else {
                    return Poll::Ready(Err(io::Error::other("the read lost its buffer")));
                };
                st.drop_stale_ahead(io, after);
                if let Some(ahead) = st.ahead.as_mut().filter(|a| !a.is_empty()) {
                    let n = crate::buf::append_spare(ahead.filled(), &mut buf);
                    *held = Some(buf);
                    ahead.consume(n);
                    st.consumed(n, after);
                    return Poll::Ready(Ok(n));
                }
                let (at, from) = (st.cursor, buf.len());
                // Read before the call, so a positional write that ends
                // while it runs is seen to have.
                let generation = io.generation();
                let task = io.submit_read_owned(buf, at, waker);
                let pooled = task.pooled();
                *job = Some(LentRead {
                    task,
                    at,
                    epoch: st.epoch,
                    generation,
                    from,
                });
                // A ring's call has its waker already, which its end wakes
                // however soon it comes: only the pool's job is polled now.
                if !pooled {
                    return Poll::Pending;
                }
            }
        })
    }

    /// Writes all of `buf` at the cursor, after the writes before it, and
    /// gives it back with its length. The job writes from `buf` itself.
    pub(crate) async fn write_owned<B>(self: &Arc<Self>, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        let mut op = WriteOwned {
            io: self,
            held: Some(OwnedBuf::written(buf)),
            out: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.held.take().unwrap_or_default().into_written())
    }
}

/// Drops `value` out of line, on a path that is never taken in practice.
#[cold]
#[inline(never)]
fn drop_cold<T>(value: T) {
    drop(value);
}

/// A write whose buffer may be out with its job. Dropped while it is, the
/// write still lands and the buffer goes with the job.
struct WriteOwned<'a> {
    io: &'a Arc<FileIo>,
    held: Option<OwnedBuf>,
    out: bool,
}

impl WriteOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let held = &mut self.held;
        let poll = self.io.with_state(cx, |st, io, waker, after| {
            st.write_owned(io, held, waker, after)
        });
        self.out = self.held.is_none();
        poll
    }
}

impl Drop for WriteOwned<'_> {
    fn drop(&mut self) {
        if self.out {
            let mut after = After::default();
            self.io
                .state
                .defer(deferred::ABANDON, &mut after, &FileHooks { io: self.io });
            after.run();
        }
    }
}
