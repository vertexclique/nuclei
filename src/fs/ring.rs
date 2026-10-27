//! A file's calls on io_uring: a read or a write at an offset, issued on
//! the ring of the shard whose thread asks for it, instead of a job of the
//! blocking pool.
//!
//! The operation keeps a copy of the file's descriptor and the buffer it
//! reads into or writes from until its last completion, so neither goes
//! while the kernel may use them. A write the kernel took only part of goes
//! on with the rest from the ring's thread. Its end is left in a slot the
//! task that waits for it polls ([`RingJob`]); a task that went away leaves
//! the end to nobody, and the buffer goes with the operation. An operation
//! cancelled because its shard went finishes its call on the blocking pool,
//! so a write that started still lands.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Poll, Waker};
use std::io;
use std::sync::Arc;

use crate::buf::PoolBuf;
use crate::io::raw::{OwnedFile, file_raw};
use crate::io::sys;
use crate::sync::atomic_waker::AtomicWaker;
use crate::syscore::linux::iouring::ring::Sqe;

/// What a file's operation reads into or writes from.
pub(crate) enum FileMem {
    /// A read into the spare room of a buffer of the pool.
    Read(PoolBuf),
    /// A read into the spare capacity of a task's buffer.
    ReadLent(Vec<u8>),
    /// A write of a buffer of the pool, `done` of its bytes landed.
    Write { buf: PoolBuf, done: usize },
    /// A write of a task's buffer, `done` of its bytes landed.
    WriteLent { buf: Vec<u8>, done: usize },
}

/// How a file's operation ended, with its buffer.
pub(crate) enum FileOutcome {
    Read(io::Result<PoolBuf>),
    ReadLent(io::Result<usize>, Vec<u8>),
    Write(io::Result<()>),
    WriteLent(io::Result<()>, Vec<u8>),
}

/// A file's operation on a ring.
pub(crate) struct FileOp {
    fd: Arc<OwnedFile>,
    /// Where the call reads or writes.
    at: u64,
    mem: FileMem,
    slot: Arc<Slot>,
}

/// Where an operation's end waits for its task.
pub(crate) struct Slot {
    full: AtomicBool,
    outcome: UnsafeCell<Option<FileOutcome>>,
    waker: AtomicWaker,
}

// SAFETY: the outcome is written once, before `full` is raised, by the
// thread that ends the operation, and taken only after `full` is seen.
unsafe impl Send for Slot {}
// SAFETY: as for `Send`.
unsafe impl Sync for Slot {}

/// Slots a thread keeps for its next calls.
const SPARE_SLOTS: usize = 8;

thread_local! {
    /// Slots of this thread's ended calls. One is used again only once the
    /// ring let go of it too: in steady state a call allocates no slot.
    static SPARES: core::cell::RefCell<Vec<Arc<Slot>>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

impl Slot {
    fn fresh() -> Self {
        Self {
            full: AtomicBool::new(false),
            outcome: UnsafeCell::new(None),
            waker: AtomicWaker::new(),
        }
    }

    /// A slot for a new call: a spare nobody else holds, made fresh, or a
    /// new one.
    fn new() -> Arc<Self> {
        let spare = SPARES.try_with(|spares| {
            let mut spares = spares.try_borrow_mut().ok()?;
            let at = spares
                .iter_mut()
                .position(|slot| Arc::get_mut(slot).is_some())?;
            let mut slot = spares.swap_remove(at);
            // Unique (`get_mut` is), so nothing else reaches it: an outcome
            // or waker left from its last call goes here.
            if let Some(only) = Arc::get_mut(&mut slot) {
                *only = Self::fresh();
            }
            Some(slot)
        });
        spare
            .ok()
            .flatten()
            .unwrap_or_else(|| Arc::new(Self::fresh()))
    }

    /// Keeps `slot`, whose task is done with it, for a later call.
    fn spare(slot: Arc<Self>) {
        let _ = SPARES.try_with(|spares| {
            if let Ok(mut spares) = spares.try_borrow_mut()
                && spares.len() < SPARE_SLOTS
            {
                if spares.capacity() == 0 {
                    spares.reserve_exact(SPARE_SLOTS);
                }
                spares.push(slot);
            }
        });
    }

    fn put(&self, outcome: FileOutcome) {
        // SAFETY: written once, before `full` is raised.
        unsafe { *self.outcome.get() = Some(outcome) };
        // Release: the task reads the outcome after it sees the flag.
        self.full.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn take(&self) -> Option<FileOutcome> {
        // Acquire: pairs with the Release of `put`.
        if !self.full.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `full` is set: the ending thread is done with the outcome.
        unsafe { (*self.outcome.get()).take() }
    }
}

/// A lent read's end: its result, and the task's buffer back.
type LentReadEnd = (io::Result<usize>, Vec<u8>);
/// A lent write's end: its result, and the task's buffer back.
type LentWriteEnd = (io::Result<()>, Vec<u8>);

/// A file's call in flight on a ring, as the task that waits for it sees it.
pub(crate) struct RingJob<T> {
    /// `None` once dropped: it went to this thread's spares.
    slot: Option<Arc<Slot>>,
    map: fn(FileOutcome) -> T,
}

impl<T> RingJob<T> {
    /// The call's end once it came, else `Pending` with `waker` woken then.
    pub(crate) fn poll(&mut self, waker: &Waker) -> Poll<T> {
        let Some(slot) = &self.slot else {
            return Poll::Pending;
        };
        if let Some(outcome) = slot.take() {
            return Poll::Ready((self.map)(outcome));
        }
        slot.waker.register(waker);
        match slot.take() {
            Some(outcome) => Poll::Ready((self.map)(outcome)),
            None => Poll::Pending,
        }
    }
}

impl<T> Drop for RingJob<T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            Slot::spare(slot);
        }
    }
}

/// Issues the call of `buf` (made into `mem`) on `fd` at `at` on the ring of
/// this thread's shard, if it runs one; else gives `buf` back for the
/// blocking pool.
fn submit<B, T>(
    fd: &Arc<OwnedFile>,
    at: u64,
    buf: B,
    mem: fn(B) -> FileMem,
    map: fn(FileOutcome) -> T,
) -> Result<RingJob<T>, B> {
    crate::rt::context::current(|current| {
        let Some(ring) = current.and_then(|shard| shard.ring()) else {
            return Err(buf);
        };
        let slot = Slot::new();
        ring.submit_file(FileOp {
            fd: Arc::clone(fd),
            at,
            mem: mem(buf),
            slot: Arc::clone(&slot),
        });
        Ok(RingJob {
            slot: Some(slot),
            map,
        })
    })
}

/// A read at `at` into the spare room of `buf`, on this thread's ring.
pub(crate) fn read(
    fd: &Arc<OwnedFile>,
    at: u64,
    buf: PoolBuf,
) -> Result<RingJob<io::Result<PoolBuf>>, PoolBuf> {
    submit(fd, at, buf, FileMem::Read, as_read)
}

/// A read at `at` into the spare capacity of a task's `buf`.
pub(crate) fn read_lent(
    fd: &Arc<OwnedFile>,
    at: u64,
    buf: Vec<u8>,
) -> Result<RingJob<LentReadEnd>, Vec<u8>> {
    submit(fd, at, buf, FileMem::ReadLent, as_read_lent)
}

/// A write of all of `buf` at `at`.
pub(crate) fn write(
    fd: &Arc<OwnedFile>,
    at: u64,
    buf: PoolBuf,
) -> Result<RingJob<io::Result<()>>, PoolBuf> {
    submit(fd, at, buf, |buf| FileMem::Write { buf, done: 0 }, as_write)
}

/// A write of all of a task's `buf` at `at`.
pub(crate) fn write_lent(
    fd: &Arc<OwnedFile>,
    at: u64,
    buf: Vec<u8>,
) -> Result<RingJob<LentWriteEnd>, Vec<u8>> {
    submit(
        fd,
        at,
        buf,
        |buf| FileMem::WriteLent { buf, done: 0 },
        as_write_lent,
    )
}

fn len32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

impl FileOp {
    /// The entry for the operation's next call. Its pointers point into the
    /// operation's buffer, whose memory does not move with the operation.
    pub(crate) fn entry(&mut self) -> Sqe {
        let fd = file_raw(&self.fd);
        match &mut self.mem {
            FileMem::Read(buf) => {
                let spare = buf.spare();
                Sqe::read(fd, spare.as_mut_ptr(), len32(spare.len()), self.at)
            }
            FileMem::ReadLent(buf) => {
                let spare = buf.spare_capacity_mut();
                Sqe::read(fd, spare.as_mut_ptr().cast(), len32(spare.len()), self.at)
            }
            FileMem::Write { buf, done } => {
                let bytes = buf.filled().get(*done..).unwrap_or_default();
                Sqe::write(
                    fd,
                    bytes.as_ptr(),
                    len32(bytes.len()),
                    self.at + *done as u64,
                )
            }
            FileMem::WriteLent { buf, done } => {
                let bytes = buf.get(*done..).unwrap_or_default();
                Sqe::write(
                    fd,
                    bytes.as_ptr(),
                    len32(bytes.len()),
                    self.at + *done as u64,
                )
            }
        }
    }

    /// The operation's call answered `res` (a count, or a negated errno):
    /// the operation again, for the rest of a write; or its end, left for
    /// its task.
    pub(crate) fn complete(mut self, res: i32) -> Option<FileOp> {
        if res == -libc::ECANCELED {
            // Its shard went: the call is made on the blocking pool.
            self.finish_on_pool();
            return None;
        }
        if let (FileMem::Write { buf, done }, Ok(n @ 1..)) = (&mut self.mem, usize::try_from(res))
            && *done + n < buf.filled().len()
        {
            *done += n;
            return Some(self);
        }
        if let (FileMem::WriteLent { buf, done }, Ok(n @ 1..)) =
            (&mut self.mem, usize::try_from(res))
            && *done + n < buf.len()
        {
            *done += n;
            return Some(self);
        }
        let result = match res {
            r if r < 0 => Err(io::Error::from_raw_os_error(-r)),
            r => Ok(r as usize),
        };
        let outcome = match self.mem {
            FileMem::Read(mut buf) => FileOutcome::Read(result.map(|n| {
                buf.set_filled(n);
                buf
            })),
            FileMem::ReadLent(mut buf) => {
                if let Ok(n) = result {
                    // SAFETY: the kernel wrote the first `n` bytes of the
                    // spare capacity.
                    unsafe { buf.set_len(buf.len() + n) };
                }
                FileOutcome::ReadLent(result, buf)
            }
            FileMem::Write { .. } => FileOutcome::Write(written(result)),
            FileMem::WriteLent { buf, .. } => FileOutcome::WriteLent(written(result), buf),
        };
        self.slot.put(outcome);
        None
    }

    /// Makes what is left of the call on the blocking pool, and leaves its
    /// end in the slot.
    fn finish_on_pool(self) {
        let FileOp { fd, at, mem, slot } = self;
        let job = crate::runtime::spawn_blocking(move || {
            let raw = file_raw(&fd);
            let outcome = match mem {
                FileMem::Read(mut buf) => {
                    FileOutcome::Read(sys::pread(raw, buf.spare(), at).map(|n| {
                        buf.set_filled(n);
                        buf
                    }))
                }
                FileMem::ReadLent(mut buf) => {
                    let result = sys::pread_uninit(raw, buf.spare_capacity_mut(), at);
                    if let Ok(n) = result {
                        // SAFETY: the call wrote the first `n` bytes of the
                        // spare capacity.
                        unsafe { buf.set_len(buf.len() + n) };
                    }
                    FileOutcome::ReadLent(result, buf)
                }
                FileMem::Write { buf, done } => FileOutcome::Write(sys::pwrite_all(
                    raw,
                    buf.filled().get(done..).unwrap_or_default(),
                    at + done as u64,
                )),
                FileMem::WriteLent { buf, done } => {
                    let result =
                        sys::pwrite_all(raw, buf.get(done..).unwrap_or_default(), at + done as u64);
                    FileOutcome::WriteLent(result, buf)
                }
            };
            slot.put(outcome);
        });
        job.detach();
    }
}

/// A write's end: it landed whole, or the kernel took nothing more.
fn written(result: io::Result<usize>) -> io::Result<()> {
    match result {
        Ok(0) => Err(io::ErrorKind::WriteZero.into()),
        Ok(_) => Ok(()),
        Err(err) => Err(err),
    }
}

/// The ends as each kind of job takes them; an end of another kind cannot
/// come (each job is made with its own), and says so if it does.
fn as_read(outcome: FileOutcome) -> io::Result<PoolBuf> {
    match outcome {
        FileOutcome::Read(result) => result,
        _ => Err(io::Error::other("a file read ended as another call")),
    }
}

fn as_read_lent(outcome: FileOutcome) -> LentReadEnd {
    match outcome {
        FileOutcome::ReadLent(result, buf) => (result, buf),
        _ => (
            Err(io::Error::other("a file read ended as another call")),
            Vec::new(),
        ),
    }
}

fn as_write(outcome: FileOutcome) -> io::Result<()> {
    match outcome {
        FileOutcome::Write(result) => result,
        _ => Err(io::Error::other("a file write ended as another call")),
    }
}

fn as_write_lent(outcome: FileOutcome) -> LentWriteEnd {
    match outcome {
        FileOutcome::WriteLent(result, buf) => (result, buf),
        _ => (
            Err(io::Error::other("a file write ended as another call")),
            Vec::new(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    use super::*;

    fn temp_file(tag: &str) -> (std::fs::File, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "nuclei-fs-ring-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        (file, path)
    }

    fn op_of(file: &std::fs::File, at: u64, mem: FileMem) -> (FileOp, Arc<Slot>) {
        let slot = Slot::new();
        let fd: OwnedFd = file.try_clone().unwrap().into();
        (
            FileOp {
                fd: Arc::new(fd),
                at,
                mem,
                slot: Arc::clone(&slot),
            },
            slot,
        )
    }

    /// A write the kernel took part of goes on with the rest, after the
    /// bytes that landed.
    #[test]
    fn a_short_write_goes_on_with_the_rest() {
        let (file, path) = temp_file("short");
        let (op, slot) = op_of(
            &file,
            10,
            FileMem::WriteLent {
                buf: b"0123456789".to_vec(),
                done: 0,
            },
        );
        let mut rest = op.complete(4).expect("the rest of the write goes on");
        match &rest.mem {
            FileMem::WriteLent { done, .. } => assert_eq!(*done, 4),
            _ => panic!("the write became another call"),
        }
        assert!(slot.take().is_none(), "the write ended early");
        // Its next entry starts after the bytes that landed.
        let entry = rest.entry();
        assert_eq!((entry.raw().off, entry.raw().len), (14, 6));
        assert!(rest.complete(6).is_none(), "the whole write landed");
        match slot.take() {
            Some(FileOutcome::WriteLent(Ok(()), buf)) => assert_eq!(buf, b"0123456789"),
            _ => panic!("the write did not end whole"),
        }
        std::fs::remove_file(path).unwrap();
    }

    /// A call cancelled because its shard went is made on the blocking pool:
    /// a write still lands, a read still reads.
    #[test]
    fn a_call_cancelled_as_its_shard_went_is_made_on_the_pool() {
        let (mut file, path) = temp_file("cancelled");
        let (op, slot) = op_of(
            &file,
            0,
            FileMem::WriteLent {
                buf: b"landed".to_vec(),
                done: 0,
            },
        );
        assert!(op.complete(-libc::ECANCELED).is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = slot.take() {
                break outcome;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pool never made the call"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(outcome, FileOutcome::WriteLent(Ok(()), _)));
        let mut got = String::new();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_to_string(&mut got).unwrap();
        assert_eq!(got, "landed");
        file.write_all(b"").unwrap();
        std::fs::remove_file(path).unwrap();
    }

    /// A file written and read back through a shard on a ring: the calls are
    /// the ring's, and the bytes come back whole and in order.
    #[test]
    fn a_file_is_read_and_written_through_the_ring() {
        use crate::rt::backend::Backend;
        use crate::rt::context;
        use crate::rt::shard::{Limits, Shard};
        use crate::syscore::linux::iouring::ring::RingConfig;
        let shard = match Shard::new(Limits {
            max_nesting: 8,
            buffer_cap: crate::buf::DEFAULT_CAP,
            backend: Backend::Ring(RingConfig::new(64)),
        }) {
            Ok(shard) => shard,
            Err(err) => {
                eprintln!("skipping: io_uring unavailable: {err}");
                return;
            }
        };
        let _current = context::enter(&shard);
        let (file, path) = temp_file("through");
        let data: Vec<u8> = (0..(3 << 20)).map(|i| (i * 7 + i / 251) as u8).collect();
        let got = shard.block_on(async {
            let mut handle = crate::io::Handle::new(file).unwrap();
            let mut rest = &data[..];
            while !rest.is_empty() {
                let n = core::future::poll_fn(|cx| {
                    futures_io::AsyncWrite::poll_write(core::pin::Pin::new(&mut handle), cx, rest)
                })
                .await
                .unwrap();
                rest = &rest[n..];
            }
            core::future::poll_fn(|cx| {
                futures_io::AsyncWrite::poll_flush(core::pin::Pin::new(&mut handle), cx)
            })
            .await
            .unwrap();
            core::future::poll_fn(|cx| {
                futures_io::AsyncSeek::poll_seek(
                    core::pin::Pin::new(&mut handle),
                    cx,
                    SeekFrom::Start(0),
                )
            })
            .await
            .unwrap();
            let mut got = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = core::future::poll_fn(|cx| {
                    futures_io::AsyncRead::poll_read(core::pin::Pin::new(&mut handle), cx, &mut buf)
                })
                .await
                .unwrap();
                if n == 0 {
                    break got;
                }
                got.extend_from_slice(&buf[..n]);
            }
        });
        assert!(got == data, "the file's bytes came back changed");
        assert!(
            shard.ring().unwrap().file_calls() > 0,
            "no call of the file went to the ring"
        );
        std::fs::remove_file(path).unwrap();
    }
}
