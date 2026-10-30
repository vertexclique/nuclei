//! The IO state of a file handle.
//!
//! A regular file is always ready to the kernel, so no poller can say when
//! its IO would not block: each read and write is a job on the blocking
//! pool, `pread` or `pwrite` at the handle's own cursor, on a copy of the
//! descriptor that every job keeps open until it ends (so a handle dropped
//! while a job runs closes only its own descriptor, and the job never meets
//! a reused number).
//!
//! One read and one write are in flight at most. A read fills a read-ahead
//! buffer of the pool, at least [`READ_MIN`] bytes, that the next reads copy
//! from. A write is copied into a buffer and accepted at once; while it is
//! in flight, later writes go into one more buffer (the write-behind), which
//! starts when the first ends, so a run of writes keeps one job going. A
//! read waits for the writes before it, so it sees them; a write or a seek
//! moves the epoch on, and a read job of an older epoch is thrown away. A
//! write that fails keeps its error: every later write fails with it until a
//! flush reports it.
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

mod work;

use core::future::poll_fn;
use core::task::{Context, Poll, Waker};
use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::task::Wake;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use self::work::{Job, Lent, LentRead, Writing, buffer, poll_task};
use crate::buf::{PoolBuf, with_pool};
use crate::io::raw::{OwnedFile, Raw, file_raw};
use crate::io::side::{After, Hooks, Side, WakerSet};
use crate::io::source::Failure;
use crate::io::sys;

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
}

/// The IO state of a file handle, shared with the waker its jobs wake.
pub(crate) struct FileIo {
    /// The jobs' copy of the descriptor.
    fd: Arc<OwnedFile>,
    state: Side<FileState>,
    /// Tasks waiting for a job: all woken when any job ends.
    waiters: WakerSet<FILE_SLOTS>,
    /// Bytes a buffered read consumed, for the state's holder to apply.
    consumed: AtomicUsize,
    /// Whether written bytes have not reached the file yet, and the cursor,
    /// as the state's last holder left them.
    unsent: AtomicBool,
    cursor: AtomicU64,
    /// The file was opened to append.
    append: bool,
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
    }

    fn releasing(&self, st: &mut FileState) {
        // Release: read without the guard, on any thread.
        self.io.unsent.store(
            st.writing.is_some() || st.behind.is_some(),
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
    /// Bytes at the cursor, read ahead.
    ahead: Option<PoolBuf>,
    reading: Option<Job<PoolBuf>>,
    writing: Option<Writing>,
    lent: Lent,
    /// Bytes accepted after the write in flight, starting at `behind_at`.
    behind: Option<PoolBuf>,
    behind_at: u64,
    failed: Option<Failure>,
    sizing: Option<crate::rt::task::Task<io::Result<u64>>>,
}

impl FileIo {
    /// The IO state of the file `fd`, starting at its own offset (a file
    /// opened by std and read some way already goes on from there).
    pub(crate) fn new(fd: Raw) -> io::Result<Arc<Self>> {
        let cursor = sys::seek(fd, None)?;
        let append = sys::appends(fd)?;
        Ok(Arc::new(Self {
            fd: Arc::new(sys::duplicate_file(fd)?),
            state: Side::new(FileState {
                cursor,
                ..FileState::default()
            }),
            waiters: WakerSet::new(),
            consumed: AtomicUsize::new(0),
            unsent: AtomicBool::new(false),
            cursor: AtomicU64::new(cursor),
            append,
            #[cfg(test)]
            on_pool: AtomicBool::new(false),
        }))
    }

    /// Sends this file's calls to the blocking pool even on a ring.
    #[cfg(test)]
    pub(crate) fn keep_on_pool(&self) {
        self.on_pool.store(true, Ordering::Relaxed);
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
                match st.fill(io, out.len(), waker, after) {
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
    /// this state that consumes, writes or seeks.
    pub(crate) fn poll_fill_buf(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(*const u8, usize)>> {
        self.with_state(cx, |st, io, waker, after| {
            st.fill(io, READ_MIN, waker, after).map_ok(|filled| {
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

    /// A read scattered into `bufs`, in order.
    pub(crate) fn poll_read_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
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
            match st.fill(io, want, waker, after) {
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
    pub(crate) async fn read_owned(self: &Arc<Self>, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        let mut held = Some(buf);
        // Dropped with the future, a job in flight is cancelled, or its
        // result thrown away: the cursor moves only when a result is taken.
        let mut job = None;
        let result = poll_fn(|cx| self.poll_read_owned(cx, &mut held, &mut job)).await;
        (result, held.unwrap_or_default())
    }

    fn poll_read_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<Vec<u8>>,
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
                    let (at, epoch, from) = (lent.at, lent.epoch, lent.from);
                    *job = None;
                    if at != st.cursor || epoch != st.epoch {
                        // Another read, a write or a seek moved on meanwhile:
                        // read again where the cursor is.
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
                if let Some(ahead) = st.ahead.as_mut().filter(|a| !a.is_empty()) {
                    let n = crate::buf::append_spare(ahead.filled(), buf);
                    ahead.consume(n);
                    st.consumed(n, after);
                    return Poll::Ready(Ok(n));
                }
                if let Some(buf) = held.take() {
                    *job = Some(io.submit_read_owned(buf, st.cursor, st.epoch));
                }
            }
        })
    }

    /// Writes all of `buf` at the cursor, after the writes before it, and
    /// gives it back with its length. The job writes from `buf` itself.
    pub(crate) async fn write_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
    ) -> (io::Result<usize>, Vec<u8>) {
        let mut op = WriteOwned {
            io: self,
            held: Some(buf),
            out: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.held.take().unwrap_or_default())
    }

    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(bytes)])
    }

    /// A write of the bytes of `bufs`, gathered in order.
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

    /// Drives the writes: takes the end of the one in flight, then starts
    /// the write-behind. Ready once none is in flight.
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
            match self.behind.take() {
                Some(behind) => self.writing = Some(io.submit_write(behind, self.behind_at)),
                None => return Poll::Ready(()),
            }
        }
    }

    /// The end of a write from a task's buffer: the buffer goes back to its
    /// task, or with the job if the task is gone.
    fn lent_back(&mut self, result: io::Result<()>, buf: Vec<u8>) -> Option<Failure> {
        let failure = result.as_ref().err().map(Failure::of);
        if matches!(self.lent, Lent::Out) {
            self.lent = Lent::Done(result, buf);
        } else {
            self.lent = Lent::None;
        }
        failure
    }

    /// Makes sure bytes at the cursor are read ahead: `true`, or `false` at
    /// the end of the file.
    fn fill(
        &mut self,
        io: &FileIo,
        want: usize,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<bool>> {
        // A read sees every byte written before it.
        if self.poll_writes(io, waker, after).is_pending() {
            return Poll::Pending;
        }
        loop {
            if self.ahead.as_ref().is_some_and(|ahead| !ahead.is_empty()) {
                return Poll::Ready(Ok(true));
            }
            if let Some(job) = self.reading.as_mut() {
                let result = match job.task.poll(waker) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                };
                let (at, epoch) = (job.at, job.epoch);
                self.reading = None;
                match result {
                    Err(err) => return Poll::Ready(Err(err)),
                    // Read before a write or a seek: not the file as it is.
                    Ok(buf) if at != self.cursor || epoch != self.epoch => after.drop_buf(buf),
                    Ok(buf) if buf.is_empty() => {
                        after.drop_buf(buf);
                        return Poll::Ready(Ok(false));
                    }
                    Ok(buf) => {
                        if let Some(old) = self.ahead.replace(buf) {
                            after.drop_buf(old);
                        }
                    }
                }
                continue;
            }
            let Some(buf) = buffer(want.max(READ_MIN), after) else {
                return Poll::Pending;
            };
            self.reading = Some(io.submit_read(buf, self.cursor, self.epoch));
        }
    }

    /// A write of all of a task's own buffer (`held`; `None` once it is
    /// submitted): once the writes before it landed, it is the write in
    /// flight, at the cursor, and Ready when it landed, with the buffer back
    /// in `held`.
    fn write_owned(
        &mut self,
        io: &FileIo,
        held: &mut Option<Vec<u8>>,
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
            self.writing = Some(io.submit_write_owned(buf, self.cursor));
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
        let len: usize = bufs.iter().map(|b| b.len()).sum();
        if len == 0 {
            return Poll::Ready(Ok(0));
        }
        self.invalidate(after);
        let n = if self.writing.is_none() {
            // Nothing in flight: these bytes go now.
            let Some(mut buf) = buffer(len.max(WRITE_MIN), after) else {
                return Poll::Pending;
            };
            let n = buf.append_vectored(bufs);
            self.writing = Some(io.submit_write(buf, self.cursor));
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
                    let fd = Arc::clone(&io.fd);
                    crate::runtime::spawn_blocking(move || sys::file_size(file_raw(&fd)))
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
    held: Option<Vec<u8>>,
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
