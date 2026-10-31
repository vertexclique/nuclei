//! The IO state of a file handle.
//!
//! A regular file is always ready to the kernel, so no poller can say when
//! its IO would not block: each read and write is a job on the blocking
//! pool, `pread` or `pwrite` at the handle's own cursor, on the handle's own
//! descriptor, of which every job holds a lease until it ends (so a handle
//! dropped while a job runs leaves its object to the job, which closes it,
//! the job never meets a reused number, and no call needs a descriptor of
//! its own: proofs/tla/lent/LentFile.tla); on io_uring an operation of the
//! shard's ring instead (see `fs::ring`). A write's job leaves its end in a
//! slot (`call`), as a ring's operation does, counted on its shard until its
//! wake went, which a runtime's drop waits for.
//!
//! One read and one write at the cursor are in flight at most. A read fills
//! a read-ahead buffer of the pool, at least [`READ_MIN`] bytes, that the
//! next reads copy from. A write is copied into a buffer and accepted at
//! once; while it is in flight, later writes go into one more buffer (the
//! write-behind), which starts when the first ends, so a run of writes keeps
//! one job going. A read waits for the writes before it, so it sees them; a
//! write or a seek moves the epoch on, and a read job of an older epoch is
//! thrown away. A write that fails keeps its error: every later write fails
//! with it until a flush reports it.
//!
//! A file opened to append writes at its end whatever its cursor says: its
//! write jobs call `write(2)` (on Windows, a write at the end), since
//! `pwrite(2)` writes at the offset given on most systems but Linux, and
//! each ends with where the file's offset is then, which the cursor takes,
//! as std's does.
//!
//! A read or a write may instead hand the job the task's own buffer
//! ([`FileIo::read_owned`], [`FileIo::write_owned`]), which comes back with
//! the result, so no byte is copied. A read into a task's buffer is the
//! task's job: it reads at the cursor once the writes before it landed, and
//! its bytes count only if the cursor and the epoch are still the ones it
//! read at (else it reads again where the cursor is); dropped, it takes the
//! cursor nowhere. A write from a task's buffer is the write in flight like
//! any other, so whatever comes after it waits for it; dropped, it still
//! lands, and its buffer goes with the job.
//!
//! Reads and writes at offsets of the caller's own (`positional`) never read
//! or move the cursor or the epoch, and run any number at once; each waits
//! for the cursor writes accepted before it. When a positional write ends
//! for its task, the file's generation moves on, and a cursor read counts a
//! job's bytes, or hands out bytes read ahead, only of the generation it
//! began in. A positional write whose task went away is kept in the state
//! (`orphans`) until it landed, and everything after it waits for it.
//!
//! A file opened for direct IO (`O_DIRECT` on Linux, `FILE_FLAG_NO_BUFFERING`
//! on Windows, as the descriptor says when the state is made) has no
//! read-ahead and no write-behind: each read or write goes to the kernel with
//! exactly the caller's length and offset, from or into memory placed in its
//! page as the caller's (see `work`), so the kernel's alignment rules give
//! the answer they would give the caller's own call. A write at the cursor
//! then waits for the one in flight instead of joining it.
//!
//! `proofs/tla/lent/LentFile.tla` checks these rules.

mod copy;
mod durable;
mod orphans;
mod positional;
mod space;
mod work;

use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};
use core::future::poll_fn;
use core::task::{Context, Poll, Waker};
use std::io::{self, SeekFrom};
use std::sync::{Arc, OnceLock};
use std::task::Wake;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use self::orphans::{Orphan, Orphans};
use self::work::{Job, Lent, LentRead, Writing, bounce, buffer, poll_task};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Hooks, Side, WakerSet};
use crate::io::source::Failure;
use crate::io::source::requests::Lease;
use crate::io::sys;

pub(crate) use self::copy::COPY_CHUNK;
#[cfg(ring)]
pub(crate) use self::durable::sync_on;
pub(crate) use self::durable::{SyncKind, sync_file};
pub(crate) use self::space::range_end;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use self::work::bounce as bounce_for_test;
#[cfg(ring)]
pub(crate) use self::work::pwrite_owned;

/// The least a read job asks for, so a run of small reads takes one job.
pub(crate) const READ_MIN: usize = 64 * 1024;
/// The least a write buffer holds, so a run of small writes takes one job.
const WRITE_MIN: usize = 64 * 1024;

/// Tasks at once that wait on one file without sharing a slot.
const FILE_SLOTS: usize = 8;

/// Work on the file state left for its holder (its deferred word).
mod deferred {
    /// Consume `consumed` bytes of the read-ahead.
    pub(super) const CONSUME: u8 = 1;
    /// The future of a lent write went away.
    pub(super) const ABANDON: u8 = 2;
    /// A positional write was left by its task (see `orphans`).
    pub(super) const ORPHAN: u8 = 4;
}

/// The IO state of a file handle, shared with the waker its jobs wake.
pub(crate) struct FileIo {
    /// The lease on the handle's descriptor its jobs share.
    fd: Lease,
    state: Side<FileState>,
    /// Tasks waiting for a job: all woken when any job ends.
    waiters: WakerSet<FILE_SLOTS>,
    /// Bytes a buffered read consumed, for the state's holder to apply.
    consumed: AtomicUsize,
    /// Whether written bytes have not reached the file yet, and the cursor,
    /// as the state's last holder left them.
    unsent: AtomicBool,
    cursor: AtomicU64,
    /// Positional writes whose end their task took (or the state took, for
    /// one its task left): the generation of what the file holds.
    generation: AtomicU64,
    /// Positional writes left by their tasks, for the state to take.
    orphans: Orphans,
    /// The first sync that failed: every later sync fails with it.
    sync_failed: OnceLock<Failure>,
    /// The file was opened to append.
    append: bool,
    /// The file was opened for direct IO.
    direct: bool,
    /// The calls go to the blocking pool even on a ring, for a test that
    /// holds the pool to keep a call in flight.
    #[cfg(test)]
    on_pool: AtomicBool,
}

/// What the file state's holder does for the work others left it.
struct FileHooks<'a> {
    io: &'a Arc<FileIo>,
}

impl Hooks<FileState> for FileHooks<'_> {
    // The file has no driver: its jobs wake the waiting tasks instead.
    fn serve(&self, _st: &mut FileState, _after: &mut After) {}

    fn deferred(&self, st: &mut FileState, bits: u8, after: &mut After) {
        if bits & deferred::CONSUME != 0 {
            let amt = self.io.consumed.swap(0, Ordering::Acquire);
            let n = st.ahead.as_mut().map_or(0, |ahead| {
                let n = amt.min(ahead.filled().len());
                ahead.consume(n);
                n
            });
            st.consumed(n, after);
        }
        if bits & deferred::ABANDON != 0 {
            st.abandon_lent(&Waker::from(Arc::clone(self.io)), after);
        }
        if bits & deferred::ORPHAN != 0 {
            self.io.orphans.take(&mut st.orphans);
        }
    }

    fn releasing(&self, st: &mut FileState) {
        // Release: read without the guard, on any thread.
        self.io.unsent.store(
            st.writing.is_some() || st.behind.is_some() || !st.orphans.is_empty(),
            Ordering::Release,
        );
        self.io.cursor.store(st.cursor, Ordering::Release);
    }
}

#[derive(Default)]
struct FileState {
    /// Where the next read or write starts.
    cursor: u64,
    /// Moves on with every write and seek.
    epoch: u64,
    /// Bytes at the cursor, read ahead, and the generation they were read in.
    ahead: Option<PoolBuf>,
    ahead_gen: u64,
    reading: Option<Job<PoolBuf>>,
    writing: Option<Writing>,
    lent: Lent,
    /// Bytes accepted after the write in flight, starting at `behind_at`.
    behind: Option<PoolBuf>,
    behind_at: u64,
    failed: Option<Failure>,
    sizing: Option<crate::rt::task::Task<io::Result<u64>>>,
    /// Positional writes left by their tasks, out until they land.
    orphans: Vec<Orphan>,
}

impl FileIo {
    /// The IO state of the file `fd`, starting at its own offset (a file
    /// opened by std and read some way already goes on from there).
    pub(crate) fn new(fd: Lease) -> io::Result<Arc<Self>> {
        let cursor = sys::seek(fd.raw(), None)?;
        let mode = sys::file_mode(fd.raw())?;
        Ok(Arc::new(Self {
            fd,
            state: Side::new(FileState {
                cursor,
                ..FileState::default()
            }),
            waiters: WakerSet::new(),
            consumed: AtomicUsize::new(0),
            unsent: AtomicBool::new(false),
            cursor: AtomicU64::new(cursor),
            generation: AtomicU64::new(0),
            orphans: Orphans::new(),
            sync_failed: OnceLock::new(),
            append: mode.append,
            direct: mode.direct,
            #[cfg(test)]
            on_pool: AtomicBool::new(false),
        }))
    }

    /// Sends this file's calls to the blocking pool even on a ring.
    #[cfg(test)]
    pub(crate) fn keep_on_pool(&self) {
        self.on_pool.store(true, Ordering::Relaxed);
    }

    /// Sends this file's calls to the shard's ring again, where it runs one.
    #[cfg(test)]
    pub(crate) fn back_to_ring(&self) {
        self.on_pool.store(false, Ordering::Relaxed);
    }

    /// The descriptor of the handle, whose calls the system's test record
    /// keeps.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn jobs_fd(&self) -> std::os::fd::RawFd {
        self.fd.raw()
    }

    /// The file's generation now.
    fn generation(&self) -> u64 {
        // Acquire: pairs with the Release of a positional write's end, so a
        // read made after it sees what it wrote.
        self.generation.load(Ordering::Acquire)
    }

    /// Runs `step` on the state for a task polling with `cx`: the task is
    /// woken when any job ends, or, if another task holds the state, when
    /// that one lets go.
    fn with_state<R>(
        self: &Arc<Self>,
        cx: &Context<'_>,
        mut step: impl FnMut(&mut FileState, &FileIo, &Waker, &mut After) -> Poll<R>,
    ) -> Poll<R> {
        self.waiters.register(cx.waker());
        let waker = Waker::from(Arc::clone(self));
        let mut after = After::default();
        let hooks = FileHooks { io: self };
        let out = self
            .state
            .poll(&mut after, cx.waker(), &hooks, |st, after| {
                step(st, self, &waker, after)
            });
        if after.starved {
            with_pool(|pool| {
                if let Some(pool) = pool {
                    pool.wait_for_return(waker.clone());
                }
            });
        }
        after.run();
        out
    }

    pub(crate) fn poll_read(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        crate::coop::poll_budget(cx, |cx| {
            self.with_state(cx, |st, io, waker, after| {
                match st.fill(io, out.len(), out.as_ptr() as usize, waker, after) {
                    Poll::Ready(Ok(true)) => {}
                    other => return other.map_ok(|_| 0),
                }
                let n = st.ahead.as_mut().map_or(0, |ahead| ahead.read_into(out));
                st.consumed(n, after);
                Poll::Ready(Ok(n))
            })
        })
    }

    /// The bytes at the cursor, read ahead: their address and length, empty
    /// at the end of the file. They stay in place until the next call on
    /// this state that consumes, writes or seeks. On a file opened for
    /// direct IO they are one read of [`READ_MIN`] bytes at the cursor.
    pub(crate) fn poll_fill_buf(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(*const u8, usize)>> {
        self.with_state(cx, |st, io, waker, after| {
            st.fill(io, READ_MIN, 0, waker, after).map_ok(|filled| {
                match st.ahead.as_ref().filter(|_| filled) {
                    Some(ahead) => (ahead.filled().as_ptr(), ahead.filled().len()),
                    None => (
                        core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const(),
                        0,
                    ),
                }
            })
        })
    }

    pub(crate) fn consume(self: &Arc<Self>, amt: usize) {
        // Release: the holder that runs the work reads the amount.
        self.consumed.fetch_add(amt, Ordering::Release);
        let mut after = After::default();
        self.state
            .defer(deferred::CONSUME, &mut after, &FileHooks { io: self });
        after.run();
    }

    /// A read scattered into `bufs`, in order. On a file opened for direct
    /// IO, a read into the first buffer that has room, as the kernel's
    /// alignment rules apply to each.
    pub(crate) fn poll_read_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.direct {
            return match bufs.iter_mut().find(|b| !b.is_empty()) {
                Some(buf) => self.poll_read(cx, buf),
                None => Poll::Ready(Ok(0)),
            };
        }
        let want: usize = bufs.iter().map(|b| b.len()).sum();
        if want == 0 {
            return Poll::Ready(Ok(0));
        }
        crate::coop::poll_budget(cx, |cx| self.poll_read_vectored_unbudgeted(cx, bufs, want))
    }

    fn poll_read_vectored_unbudgeted(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        want: usize,
    ) -> Poll<io::Result<usize>> {
        self.with_state(cx, |st, io, waker, after| {
            match st.fill(io, want, 0, waker, after) {
                Poll::Ready(Ok(true)) => {}
                other => return other.map_ok(|_| 0),
            }
            let n = st
                .ahead
                .as_mut()
                .map_or(0, |ahead| ahead.read_into_vectored(bufs));
            st.consumed(n, after);
            Poll::Ready(Ok(n))
        })
    }

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
                    let (result, mut buf) = match lent.task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(out) => out,
                    };
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
                let Some(buf) = held.as_mut() else {
                    return Poll::Ready(Err(io::Error::other("the read lost its buffer")));
                };
                st.drop_stale_ahead(io, after);
                if let Some(ahead) = st.ahead.as_mut().filter(|a| !a.is_empty()) {
                    let n = crate::buf::append_spare(ahead.filled(), buf);
                    ahead.consume(n);
                    st.consumed(n, after);
                    return Poll::Ready(Ok(n));
                }
                if let Some(buf) = held.take() {
                    let (at, from) = (st.cursor, buf.len());
                    // Read before the call, so a positional write that ends
                    // while it runs is seen to have.
                    let generation = io.generation();
                    *job = Some(LentRead {
                        task: io.submit_read_owned(buf, at),
                        at,
                        epoch: st.epoch,
                        generation,
                        from,
                    });
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

    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(bytes)])
    }

    /// A write of the bytes of `bufs`, gathered in order (on a file opened
    /// for direct IO, of the first one that has any).
    pub(crate) fn poll_write_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |cx| {
            self.with_state(cx, |st, io, waker, after| st.write(io, bufs, waker, after))
        })
    }

    pub(crate) fn poll_flush(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.with_state(cx, |st, io, waker, after| {
            match st.poll_writes(io, waker, after) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => Poll::Ready(st.failed.take().map_or(Ok(()), |f| Err(f.error()))),
            }
        })
    }

    pub(crate) fn poll_seek(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        self.with_state(cx, |st, io, waker, after| st.seek(io, pos, waker, after))
    }

    /// Whether written bytes have not reached the file yet, as the state's
    /// last holder left it.
    pub(crate) fn has_unsent(&self) -> bool {
        self.unsent.load(Ordering::Acquire)
    }

    /// The cursor, where the file's own offset goes when the handle hands
    /// the file out, as the state's last holder left it.
    pub(crate) fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }
}

/// A job ended (or a buffer came back): every task waiting on the file
/// looks again.
impl Wake for FileIo {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut after = After::default();
        self.waiters.take_all(&mut after);
        after.run();
    }
}

impl FileState {
    /// Bytes read ahead, and reads in flight, are of the file as it was.
    fn invalidate(&mut self, after: &mut After) {
        if let Some(ahead) = self.ahead.take() {
            after.drop_buf(ahead);
        }
        self.epoch += 1;
    }

    fn consumed(&mut self, n: usize, after: &mut After) {
        self.cursor += n as u64;
        if self.ahead.as_ref().is_some_and(PoolBuf::is_empty)
            && let Some(done) = self.ahead.take()
        {
            after.drop_buf(done);
        }
    }

    /// Whether the bytes of a read at `at` in `epoch` and `generation` are the
    /// file's at the cursor now: no read, write or seek moved the cursor or
    /// the epoch on, and no positional write ended, since.
    fn counts(&self, io: &FileIo, at: u64, epoch: u64, generation: u64) -> bool {
        at == self.cursor && epoch == self.epoch && generation == io.generation()
    }

    /// Bytes read ahead before a positional write ended are not the file's.
    fn drop_stale_ahead(&mut self, io: &FileIo, after: &mut After) {
        if self.ahead.is_some()
            && self.ahead_gen != io.generation()
            && let Some(stale) = self.ahead.take()
        {
            after.drop_buf(stale);
        }
    }

    /// Drives the writes: takes the end of the one in flight, then of the
    /// positional writes left by their tasks, then starts the write-behind.
    /// Ready once none is in flight.
    fn poll_writes(&mut self, io: &FileIo, waker: &Waker, after: &mut After) -> Poll<()> {
        loop {
            if let Some(writing) = self.writing.as_mut() {
                // Where an append left the file's offset: the cursor.
                let (failure, end) = match writing {
                    Writing::Pool(task) => match task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => (result.as_ref().err().map(Failure::of), None),
                    },
                    Writing::Append(task) => match task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => {
                            (result.as_ref().err().map(Failure::of), result.ok())
                        }
                    },
                    Writing::Lent(task) => match task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready((result, buf)) => (self.lent_back(result, buf), None),
                    },
                    Writing::AppendLent(task) => match task.poll(waker) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready((result, buf)) => {
                            let end = result.as_ref().ok().copied();
                            (self.lent_back(result.map(drop), buf), end)
                        }
                    },
                };
                self.writing = None;
                if let Some(end) = end {
                    self.cursor = end;
                }
                if let Some(failure) = failure {
                    self.failed = Some(failure);
                    if let Some(behind) = self.behind.take() {
                        after.drop_buf(behind);
                    }
                }
            }
            if self.poll_orphans(io, waker).is_pending() {
                return Poll::Pending;
            }
            match self.behind.take() {
                Some(behind) => self.writing = Some(io.submit_write(behind, self.behind_at, waker)),
                None => return Poll::Ready(()),
            }
        }
    }

    /// Takes the ends of the positional writes left by their tasks: Ready
    /// once every one landed. Each that landed moves the generation on, and
    /// one that failed is the file's failure, as a lent write's nobody took.
    fn poll_orphans(&mut self, io: &FileIo, waker: &Waker) -> Poll<()> {
        io.orphans.take(&mut self.orphans);
        if self.orphans.is_empty() {
            return Poll::Ready(());
        }
        let before = self.orphans.len();
        let failed = &mut self.failed;
        self.orphans.retain_mut(|orphan| match orphan.poll(waker) {
            Poll::Pending => true,
            Poll::Ready(err) => {
                if let Some(err) = err {
                    *failed = Some(Failure::of(&err));
                }
                false
            }
        });
        let landed = before - self.orphans.len();
        if landed > 0 {
            // Release: pairs with the Acquire of `FileIo::generation`.
            io.generation.fetch_add(landed as u64, Ordering::Release);
        }
        if self.orphans.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// The end of a write from a task's buffer: the buffer goes back to its
    /// task, or with the job if the task is gone.
    fn lent_back(&mut self, result: io::Result<()>, buf: OwnedBuf) -> Option<Failure> {
        let failure = result.as_ref().err().map(Failure::of);
        if matches!(self.lent, Lent::Out) {
            self.lent = Lent::Done(result, buf);
        } else {
            self.lent = Lent::None;
        }
        failure
    }

    /// Makes sure bytes at the cursor are read ahead: `true`, or `false` at
    /// the end of the file. A read job asks for `want` bytes, at least
    /// [`READ_MIN`]; on a file opened for direct IO exactly `want`, into
    /// memory placed in its page as the caller's at `addr`.
    fn fill(
        &mut self,
        io: &FileIo,
        want: usize,
        addr: usize,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<bool>> {
        // A read sees every byte written before it.
        if self.poll_writes(io, waker, after).is_pending() {
            return Poll::Pending;
        }
        loop {
            self.drop_stale_ahead(io, after);
            if self.ahead.as_ref().is_some_and(|ahead| !ahead.is_empty()) {
                return Poll::Ready(Ok(true));
            }
            if let Some(job) = self.reading.as_mut() {
                let result = match job.task.poll(waker) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                };
                let (at, epoch, generation) = (job.at, job.epoch, job.generation);
                self.reading = None;
                match result {
                    Err(err) => return Poll::Ready(Err(err)),
                    // Read before a write, a seek or a positional write's
                    // end: not the file as it is.
                    Ok(buf) if !self.counts(io, at, epoch, generation) => after.drop_buf(buf),
                    Ok(buf) if buf.is_empty() => {
                        after.drop_buf(buf);
                        return Poll::Ready(Ok(false));
                    }
                    Ok(buf) => {
                        if let Some(old) = self.ahead.replace(buf) {
                            after.drop_buf(old);
                        }
                        self.ahead_gen = generation;
                    }
                }
                continue;
            }
            let (buf, from, len) = if io.direct {
                match bounce(want, addr, true, waker) {
                    Some(got) => got,
                    None => return Poll::Pending,
                }
            } else {
                let Some(buf) = buffer(want.max(READ_MIN), after) else {
                    return Poll::Pending;
                };
                let len = buf.capacity();
                (buf, 0, len)
            };
            // Read before the call, so a positional write that ends while it
            // runs is seen to have.
            let generation = io.generation();
            self.reading = Some(Job {
                task: io.submit_read(buf, from, len, self.cursor),
                at: self.cursor,
                epoch: self.epoch,
                generation,
            });
        }
    }

    /// A write of all of a task's own buffer (`held`; `None` once it is
    /// submitted): once the writes before it landed, it is the write in
    /// flight, at the cursor, and Ready when it landed, with the buffer back
    /// in `held`.
    fn write_owned(
        &mut self,
        io: &FileIo,
        held: &mut Option<OwnedBuf>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(buf) = held.as_ref() {
            // Another task's buffer is out: this one waits its turn.
            if !matches!(self.lent, Lent::None) {
                let _ = self.poll_writes(io, waker, after);
                return Poll::Pending;
            }
            if self.poll_writes(io, waker, after).is_pending() {
                return Poll::Pending;
            }
            if let Some(failure) = self.failed {
                return Poll::Ready(Err(failure.error()));
            }
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let Some(buf) = held.take() else {
                return Poll::Pending;
            };
            let len = buf.len() as u64;
            self.invalidate(after);
            self.writing = Some(io.submit_write_owned(buf, self.cursor, waker));
            self.cursor += len;
            self.lent = Lent::Out;
        }
        let _ = self.poll_writes(io, waker, after);
        match core::mem::take(&mut self.lent) {
            Lent::Done(result, buf) => {
                let len = buf.len();
                *held = Some(buf);
                // Writes that wait for a turn go on.
                after.wake_one(waker.clone());
                Poll::Ready(result.map(|()| len))
            }
            other => {
                self.lent = other;
                Poll::Pending
            }
        }
    }

    /// The task of a submitted write from its own buffer went away: the
    /// write still lands, and an error nobody took is the write's failure.
    fn abandon_lent(&mut self, waker: &Waker, after: &mut After) {
        match core::mem::take(&mut self.lent) {
            Lent::Out => self.lent = Lent::Orphan,
            Lent::Done(Err(err), _) => self.failed = Some(Failure::of(&err)),
            _ => {}
        }
        after.wake_one(waker.clone());
    }

    fn write(
        &mut self,
        io: &FileIo,
        bufs: &[io::IoSlice<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let _ = self.poll_writes(io, waker, after);
        if let Some(failure) = self.failed {
            return Poll::Ready(Err(failure.error()));
        }
        if io.direct {
            return self.write_direct(io, bufs, waker, after);
        }
        let len: usize = bufs.iter().map(|b| b.len()).sum();
        if len == 0 {
            return Poll::Ready(Ok(0));
        }
        self.invalidate(after);
        let n = if self.writing.is_none() && self.orphans.is_empty() {
            // Nothing in flight: these bytes go now.
            let Some(mut buf) = buffer(len.max(WRITE_MIN), after) else {
                return Poll::Pending;
            };
            let n = buf.append_vectored(bufs);
            self.writing = Some(io.submit_write(buf, self.cursor, waker));
            n
        } else if let Some(behind) = self.behind.as_mut() {
            match behind.append_vectored(bufs) {
                // Both buffers full: the write waits for the one in flight.
                0 => return Poll::Pending,
                n => n,
            }
        } else {
            let Some(mut buf) = buffer(len.max(WRITE_MIN), after) else {
                return Poll::Pending;
            };
            let n = buf.append_vectored(bufs);
            self.behind = Some(buf);
            self.behind_at = self.cursor;
            n
        };
        self.cursor += n as u64;
        // The write in flight is polled once more, a write just started
        // with it: its end wakes the file's waiters (this task among them)
        // whether or not the task polls the file again, so a task that
        // parks holding the file is not kept by its own registration.
        let _ = self.poll_writes(io, waker, after);
        Poll::Ready(Ok(n))
    }

    /// A write at the cursor of a file opened for direct IO: the bytes of
    /// the first buffer that has any, as one call of exactly them, once the
    /// write in flight landed. Accepted as it is submitted; its error, as
    /// any write's, fails the next write or the flush.
    fn write_direct(
        &mut self,
        io: &FileIo,
        bufs: &[io::IoSlice<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let Some(bytes) = bufs.iter().find(|b| !b.is_empty()) else {
            return Poll::Ready(Ok(0));
        };
        if self.writing.is_some() || !self.orphans.is_empty() {
            return Poll::Pending;
        }
        let Some((mut buf, from, n)) = bounce(bytes.len(), bytes.as_ptr() as usize, true, waker)
        else {
            return Poll::Pending;
        };
        buf.fill_at(from, &bytes[..n]);
        self.invalidate(after);
        self.writing = Some(io.submit_write(buf, self.cursor, waker));
        self.cursor += n as u64;
        Poll::Ready(Ok(n))
    }

    fn seek(
        &mut self,
        io: &FileIo,
        pos: SeekFrom,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<u64>> {
        // Writes land first, so the cursor and the size are the file's.
        if self.poll_writes(io, waker, after).is_pending() {
            return Poll::Pending;
        }
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.cursor.checked_add_signed(d),
            SeekFrom::End(d) => {
                let task = self.sizing.get_or_insert_with(|| {
                    let fd = io.fd.clone();
                    crate::runtime::spawn_blocking(move || sys::file_size(fd.raw()))
                });
                let size = match poll_task(task, waker) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(size) => size,
                };
                self.sizing = None;
                match size {
                    Ok(size) => size.checked_add_signed(d),
                    Err(err) => {
                        crate::rt::counters::io_error(&err);
                        return Poll::Ready(Err(err));
                    }
                }
            }
        };
        let Some(target) = target else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the seek goes before the start of the file",
            )));
        };
        if target != self.cursor {
            self.invalidate(after);
            self.cursor = target;
        }
        Poll::Ready(Ok(target))
    }
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
