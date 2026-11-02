//! Syncing a file: on io_uring an `IORING_OP_FSYNC` of the shard's ring
//! (with `IORING_FSYNC_DATASYNC` for a sync of the data), elsewhere a job of
//! the blocking pool calling std's own sync; either way on the handle's own
//! descriptor, leased for the call.
//!
//! A sync covers every write accepted at the cursor before it was called and
//! every positional write that returned before it: it waits until the
//! write-behind went out and every write in flight (and every positional
//! write left by its task) landed, then makes its call. Waiting for their
//! ends orders the sync after them on every backend; the ring's
//! `IOSQE_IO_DRAIN` would not (a write its shard handed to the blocking pool
//! is not the ring's), and would hold the sync behind every other operation
//! of the ring, a multishot receive among them, which may never end
//! (`proofs/tla/lent/LentFile.tla`, scenario "sync", `MF_sync_drain`).
//!
//! A write that failed is reported by the sync, as by a flush (and then
//! forgotten, as a flush forgets it). A failed sync is reported, and every
//! sync of the handle called after it fails with the same error without a
//! call: the kernel reports a write-back error to one call only, so a later
//! call that succeeded would say nothing of the data that did not reach the
//! device. So that no failure the kernel reports goes unseen, the calls are
//! the file state's, not their tasks' ([`Syncs`]): the handle makes one at a
//! time, a sync whose call would go beside another's waiting for that one's
//! end (two at once, the kernel could report the error to one and the other's
//! task look first, `MF_sync_unserialized`), and a task that goes away with
//! its call out (a timeout dropped its future) leaves the call to the state,
//! whose next sync takes its end and its failure (`MF_sync_task_records`). A
//! sync fails for its own call's failure and for one of a call made before
//! its own, never for a later call's (`MF_sync_any_failure`).
//!
//! A sync of a range of the file (`sync_range`) is the same call with
//! another kind: on Linux `sync_file_range(2)` (an `IORING_OP_SYNC_FILE_RANGE`
//! on the ring, for a range its length field holds), which writes the range's
//! pages back and waits for them but flushes no metadata and no device cache;
//! elsewhere the sync of the file's data. It waits for the writes it covers as
//! the others do, and its failure is the handle's sticky one: the kernel
//! reports a write-back error it meets to that call alone.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::io;
use std::sync::Arc;

use super::FileIo;
#[cfg(ring)]
use crate::fs::call::SlotJob;
use crate::io::source::Failure;
use crate::io::source::requests::Lease;
use crate::rt::task::Join;

/// What a sync writes through.
#[derive(Clone, Copy)]
pub(crate) enum SyncKind {
    /// The data and all the metadata.
    All,
    /// The data, and the metadata a read needs.
    Data,
    /// The pages of `len` bytes from `offset` (zero: to the file's end).
    Range {
        // Only Linux's call reads the range: elsewhere the file's data is synced.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        offset: u64,
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        len: u64,
    },
}

impl FileIo {
    /// Writes what `kind` says through, once every write it covers landed.
    pub(crate) async fn sync(self: &Arc<Self>, kind: SyncKind) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx)).await?;
        let mut mine = 0;
        poll_fn(|cx| {
            self.with_state(cx, |st, io, waker, _| {
                st.syncs.poll(io, kind, &mut mine, waker)
            })
        })
        .await
    }

    /// A sync call of `kind`: on the ring of this thread's shard where it
    /// runs one, else a job of the blocking pool.
    fn sync_call(&self, kind: SyncKind) -> SyncCall {
        #[cfg(ring)]
        if self.on_ring() {
            let job = match kind {
                SyncKind::All => crate::fs::ring::sync(&self.fd, false),
                SyncKind::Data => crate::fs::ring::sync(&self.fd, true),
                // The entry's length field is 32 bits: a longer range goes to the pool.
                SyncKind::Range { offset, len } => match u32::try_from(len) {
                    Ok(len) => crate::fs::ring::sync_range(&self.fd, offset, len),
                    Err(_) => Err(()),
                },
            };
            if let Ok(job) = job {
                return SyncCall::Slot(job);
            }
        }
        let fd = self.fd.clone();
        #[cfg(test)]
        let fails = self.sync_fails();
        SyncCall::Pool(
            crate::runtime::spawn_blocking(move || {
                #[cfg(test)]
                if fails {
                    return Err(io::Error::other("the device lost a write-back"));
                }
                sync_on(&fd, kind)
            })
            .join(),
        )
    }
}

/// The handle's sync calls, kept by the file state rather than by the tasks
/// that make them: one out at a time, and the number of the first that
/// failed (`proofs/tla/lent/LentFile.tla`, scenario "sync").
#[derive(Default)]
pub(super) struct Syncs {
    /// The call out, or ended and not taken yet, and its number.
    out: Option<(u64, SyncCall)>,
    /// How many calls were made: the number of the last one.
    made: u64,
    /// The number of the last call whose end was taken.
    ended: u64,
    /// The first call that failed, and its failure: every sync whose own
    /// call came after it, or was it, fails, and so does every sync called
    /// after it, without a call.
    failed: Option<(u64, Failure)>,
}

/// A sync call in flight: an operation of the ring that ends in a slot, or a
/// job of the blocking pool.
enum SyncCall {
    Pool(Join<io::Result<()>>),
    #[cfg(ring)]
    Slot(SlotJob<io::Result<()>>),
}

impl SyncCall {
    /// The call's end once it came, its error counted for `stats`; one the
    /// runtime lost before it ended is a failure whose call may have run.
    fn poll(&mut self, waker: &Waker) -> Poll<io::Result<()>> {
        let end = match self {
            SyncCall::Pool(job) => match Pin::new(job).poll(&mut Context::from_waker(waker)) {
                Poll::Ready(Ok(end)) => end,
                Poll::Ready(Err(lost)) => return Poll::Ready(Err(lost.into())),
                Poll::Pending => return Poll::Pending,
            },
            #[cfg(ring)]
            SyncCall::Slot(job) => core::task::ready!(job.poll(waker)),
        };
        Poll::Ready(end.inspect_err(crate::rt::counters::io_error))
    }
}

impl Syncs {
    /// A sync of `kind` whose own call is numbered `mine` (0 until it made
    /// one), under the file state: first the end of the call out, whoever's,
    /// is taken (`waker` is woken when it comes); then the sync answers if
    /// its own call ended, fails if a call failed, or makes its call.
    pub(super) fn poll(
        &mut self,
        io: &FileIo,
        kind: SyncKind,
        mine: &mut u64,
        waker: &Waker,
    ) -> Poll<io::Result<()>> {
        loop {
            if *mine != 0 && self.ended >= *mine {
                return Poll::Ready(match self.failed {
                    Some((at, failure)) if at <= *mine => Err(failure.error()),
                    _ => Ok(()),
                });
            }
            if self.out.is_some() {
                core::task::ready!(self.take(waker));
                continue;
            }
            if let Some((_, failure)) = self.failed {
                return Poll::Ready(Err(failure.error()));
            }
            self.made += 1;
            *mine = self.made;
            self.out = Some((self.made, io.sync_call(kind)));
        }
    }

    /// Takes the end of the call out once it came, keeping its failure if
    /// it is the first.
    fn take(&mut self, waker: &Waker) -> Poll<()> {
        let Some((at, call)) = self.out.as_mut() else {
            return Poll::Ready(());
        };
        let end = core::task::ready!(call.poll(waker));
        self.ended = *at;
        if let Err(err) = end
            && self.failed.is_none()
        {
            self.failed = Some((*at, Failure::of(&err)));
        }
        self.out = None;
        Poll::Ready(())
    }
}

/// std's sync of the file `fd` (`fdatasync` for `Data`, else `fsync`;
/// `F_FULLFSYNC` on Apple's systems; `FlushFileBuffers` on Windows), or the
/// write-back of a range.
pub(crate) fn sync_on(fd: &Lease, kind: SyncKind) -> io::Result<()> {
    sync_file(&fd.file(), kind)
}

/// As [`sync_on`], on a file at hand. A range is written back and waited for
/// by `sync_file_range(2)` on Linux, which a stream refuses as the kernel
/// does; elsewhere it is the sync of the file's data, which covers it.
pub(crate) fn sync_file(file: &std::fs::File, kind: SyncKind) -> io::Result<()> {
    match kind {
        SyncKind::All => file.sync_all(),
        #[cfg(not(target_os = "linux"))]
        SyncKind::Data | SyncKind::Range { .. } => file.sync_data(),
        #[cfg(target_os = "linux")]
        SyncKind::Data => file.sync_data(),
        #[cfg(target_os = "linux")]
        SyncKind::Range { offset, len } => {
            use std::os::fd::AsRawFd;
            crate::io::sys::sync_file_range(file.as_raw_fd(), offset, len)
        }
    }
}
