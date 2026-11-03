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
//! A write that failed is reported by the sync, as by a flush (which then
//! forgets it, as a stream does). A failed sync is reported, and every sync of
//! the handle or a clone called after it fails with the same error without a
//! call: the kernel reports a write-back error to one call only, so a later
//! call that succeeded would say nothing of the data that did not reach the
//! device. Both
//! are kept in the handle's [`Lost`], which no report forgets: a write the
//! handle accepted and the kernel then failed (whoever took its failure: a
//! flush, a call beside the reads and writes, `get_mut`, the drop), and a sync
//! call that failed. Every sync reads it before it makes a call, and fails with
//! it. The handle keeps it across `get_mut`, and its clones (`try_clone`, the
//! same open file, whose error the kernel reports to one of them) share it.
//! So that no failure the kernel reports goes unseen, the calls are the
//! record's, not their tasks' ([`Syncs`]), and the record is the handle's and
//! its clones' together: they make one call at a time across them all, a sync
//! whose call would go beside another's (the handle's or a clone's) waiting for
//! that one's end. The kernel reports a lost write-back of the open file to one
//! call only, so a call made beside another could find no error and answer Ok
//! for writes that never reached the device, though the other call took it
//! (`proofs/tla/lent/LentFile.tla`, scenario "sync_clone_race", mutation
//! `syncs_per_handle`; for one handle, `MF_sync_unserialized`). A task that
//! goes away with its call out (a timeout dropped its future) leaves the call
//! to the record, whose next sync, of either handle, takes its end and its
//! failure (`MF_sync_task_records`). A sync fails for its own call's failure
//! and for one of a call made before its own, never for a later call's
//! (`MF_sync_any_failure`).
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
use std::task::Wake;

use super::{FILE_SLOTS, FileIo};
#[cfg(ring)]
use crate::fs::call::SlotJob;
use crate::io::side::{After, Hooks, Side, WakerSet};
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
        poll_fn(|cx| self.lost.poll_sync(self, kind, &mut mine, cx.waker())).await
    }

    /// How many sync calls the record shared with this handle's clones made.
    #[cfg(test)]
    pub(crate) fn sync_calls_made(&self) -> u64 {
        self.lost.made.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Fails the next `n` sync calls made on the blocking pool by this handle
    /// or its clones, as a device that lost a write-back would, without a
    /// call: the system reports it to whichever call looks first.
    #[cfg(test)]
    pub(crate) fn fail_next_syncs(&self, n: u32) {
        self.lost
            .fail_syncs
            .store(n, core::sync::atomic::Ordering::Relaxed);
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
        let fails = self.lost.sync_fails();
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

/// The sync calls of a handle and its clones, kept by their shared record
/// rather than by the tasks that make them: one out at a time across all of
/// them, and the number of the first that failed
/// (`proofs/tla/lent/LentFile.tla`, scenarios "sync" and "sync_clone_race").
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
    /// after it, without a call (through the [`Lost`]).
    failed: Option<(u64, Failure)>,
}

/// What the syncs of a handle and its clones answer once a byte it accepted
/// may not be on the device: the first write the kernel failed after the
/// handle accepted it, or the first sync call that failed. Kept for good, by
/// the handle across `get_mut` and by its clones, which share one. It holds
/// their sync calls too, one at a time across them all: the kernel reports a
/// lost write-back of the open file to one call only, so a call beside
/// another's could answer Ok after the other took the error
/// (`proofs/tla/lent/LentFile.tla`, scenarios "sync_lost", "sync_clone" and
/// "sync_clone_race").
pub(crate) struct Lost {
    failure: std::sync::OnceLock<Failure>,
    /// The calls, behind the guard.
    calls: Side<Syncs>,
    /// Tasks waiting on a call of any of the handles: all woken when a call ends.
    waiters: WakerSet<FILE_SLOTS>,
    /// How many calls were made, for a test.
    #[cfg(test)]
    made: core::sync::atomic::AtomicU64,
    /// How many of the next sync calls on the blocking pool fail without a
    /// call, for a test of a write-back the device lost.
    #[cfg(test)]
    fail_syncs: core::sync::atomic::AtomicU32,
}

impl Default for Lost {
    fn default() -> Self {
        Self {
            failure: std::sync::OnceLock::new(),
            calls: Side::new(Syncs::default()),
            waiters: WakerSet::new(),
            #[cfg(test)]
            made: core::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            fail_syncs: core::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl Lost {
    /// Keeps `failure`, unless one is kept already.
    pub(crate) fn keep(&self, failure: Failure) {
        let _ = self.failure.set(failure);
    }

    /// The failure kept, if any.
    pub(crate) fn get(&self) -> Option<Failure> {
        self.failure.get().copied()
    }

    /// One poll of a sync of `kind` made through `io`, whose own call is
    /// numbered `mine` (0 until it made one): the task is woken when any call
    /// of the record ends, or, if another task holds the calls, when that one
    /// lets go.
    fn poll_sync(
        self: &Arc<Self>,
        io: &FileIo,
        kind: SyncKind,
        mine: &mut u64,
        task: &Waker,
    ) -> Poll<io::Result<()>> {
        self.waiters.register(task);
        let waker = Waker::from(Arc::clone(self));
        let mut after = After::default();
        let out = self.calls.poll(&mut after, task, &CallHooks, |calls, _| {
            calls.poll(self, io, kind, mine, &waker)
        });
        after.run();
        out
    }

    /// Whether the sync call made now is one [`FileIo::fail_next_syncs`] fails.
    #[cfg(test)]
    fn sync_fails(&self) -> bool {
        self.fail_syncs
            .try_update(
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
                |n| n.checked_sub(1),
            )
            .is_ok()
    }
}

impl Wake for Lost {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut after = After::default();
        self.waiters.take_all(&mut after);
        after.run();
    }
}

/// What the holder of the calls does for the work others left it: nothing,
/// the record has no driver and its calls wake the waiting tasks instead.
struct CallHooks;

impl Hooks<Syncs> for CallHooks {
    fn serve(&self, _st: &mut Syncs, _after: &mut After) {}

    // The record's side takes through its word only (`Side::new`).
    fn releases_inline(&self) -> bool {
        true
    }
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
    /// one), under the record's guard: first the end of the call out, whoever's,
    /// is taken (`waker` is woken when it comes); then the sync answers if
    /// its own call ended, fails if a call failed, or makes its call.
    pub(super) fn poll(
        &mut self,
        lost: &Lost,
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
                core::task::ready!(self.take(lost, waker));
                continue;
            }
            if let Some(failure) = lost.get() {
                return Poll::Ready(Err(failure.error()));
            }
            self.made += 1;
            #[cfg(test)]
            lost.made
                .store(self.made, core::sync::atomic::Ordering::Relaxed);
            *mine = self.made;
            self.out = Some((self.made, io.sync_call(kind)));
        }
    }

    /// Takes the end of the call out once it came, keeping its failure if
    /// it is the first, in the record too.
    fn take(&mut self, lost: &Lost, waker: &Waker) -> Poll<()> {
        let Some((at, call)) = self.out.as_mut() else {
            return Poll::Ready(());
        };
        let end = core::task::ready!(call.poll(waker));
        self.ended = *at;
        if let Err(err) = end
            && self.failed.is_none()
        {
            let failure = Failure::of(&err);
            self.failed = Some((*at, failure));
            lost.keep(failure);
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
