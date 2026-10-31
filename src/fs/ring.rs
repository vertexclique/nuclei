//! A file's calls on io_uring: a read or a write at an offset, or a sync,
//! issued on the ring of the shard whose thread asks for it, instead of a
//! job of the blocking pool.
//!
//! The operation keeps a lease on the handle's descriptor and the buffer it
//! reads into or writes from until its last completion, so neither goes
//! while the kernel may use them (and the number reaches no other file while
//! the entry waits for room). A write the kernel took only part of goes
//! on with the rest from the ring's thread. Its end is left in a slot the
//! task that waits for it polls (`call`); a task that went away leaves the
//! end to nobody, and the buffer goes with the operation. A write is counted
//! on its shard until its end's wake went (`call::WriteCall`). An operation
//! cancelled because its shard went finishes its call on the blocking pool
//! (or, on a thread inside no runtime, there), so a write that started still
//! lands.

use std::io;
use std::sync::Arc;

use super::Allocate;
use super::call::{
    FileOutcome, LentReadEnd, LentWriteEnd, Slot, SlotJob, WriteCall, as_read, as_read_lent,
    as_sync, as_write, as_write_lent,
};
use super::file::{SyncKind, sync_on};
use crate::buf::PoolBuf;
use crate::buf::owned::OwnedBuf;
use crate::io::source::requests::Lease;
use crate::io::sys;
use crate::syscore::linux::iouring::ring::Sqe;

/// What a file's operation reads into or writes from.
pub(crate) enum FileMem {
    /// A read into `len` bytes of a buffer of the pool from `from` (each
    /// within the buffer's `u32` capacity, so the variant is no larger than
    /// a write's).
    Read { buf: PoolBuf, from: u32, len: u32 },
    /// A read into the spare capacity of a task's buffer.
    ReadLent(OwnedBuf),
    /// A write of a buffer of the pool, `done` of its bytes landed.
    Write { buf: PoolBuf, done: usize },
    /// A write of a task's buffer, `done` of its bytes landed.
    WriteLent { buf: OwnedBuf, done: usize },
    /// A sync of the file's data (`data_only`) or of all of it.
    Sync { data_only: bool },
    /// A change of the space `len` bytes from `at` use, as `mode` says.
    Allocate { len: u64, mode: Allocate },
    /// A write-back of `len` bytes from `at` (zero: to the file's end).
    SyncRange { len: u32 },
}

/// The flags of a range write-back: wait before, write, and wait after
/// (`SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE |
/// SYNC_FILE_RANGE_WAIT_AFTER`, the kernel's own numbers).
const SYNC_RANGE_FLAGS: u32 = 1 | 2 | 4;

/// A file's operation on a ring.
pub(crate) struct FileOp {
    fd: Lease,
    /// Where the call reads or writes.
    at: u64,
    mem: FileMem,
    slot: Arc<Slot>,
    /// A write's count on the shard that issued it; let go of after the
    /// slot was filled, its wake gone.
    write: Option<WriteCall>,
}

/// Issues the call of `buf` (made into `mem`) on `fd` at `at` on the ring of
/// this thread's shard, if it runs one; else gives `buf` back for the
/// blocking pool.
fn submit<B, T>(
    fd: &Lease,
    at: u64,
    buf: B,
    mem: fn(B) -> FileMem,
    map: fn(FileOutcome) -> T,
) -> Result<SlotJob<T>, B> {
    crate::rt::context::current(|current| {
        let Some((shard, ring)) = current.and_then(|shard| Some((shard, shard.ring()?))) else {
            return Err(buf);
        };
        let slot = Slot::new();
        let mem = mem(buf);
        let write = matches!(
            mem,
            FileMem::Write { .. } | FileMem::WriteLent { .. } | FileMem::Allocate { .. }
        )
        .then(|| WriteCall::on(&shard.shared));
        ring.submit_file(FileOp {
            fd: fd.clone(),
            at,
            mem,
            slot: Arc::clone(&slot),
            write,
        });
        Ok(SlotJob::new(slot, map))
    })
}

/// A read at `at` into `len` bytes of `buf` from `from`, on this thread's
/// ring.
pub(crate) fn read(
    fd: &Lease,
    at: u64,
    buf: PoolBuf,
    from: usize,
    len: usize,
) -> Result<SlotJob<io::Result<PoolBuf>>, PoolBuf> {
    submit(
        fd,
        at,
        (buf, from, len),
        |(buf, from, len)| FileMem::Read {
            buf,
            from: u32::try_from(from).unwrap_or(u32::MAX),
            len: u32::try_from(len).unwrap_or(u32::MAX),
        },
        as_read,
    )
    .map_err(|(buf, _, _)| buf)
}

/// A sync of the file's data (`data_only`) or of all of it, on this
/// thread's ring; `Err` when the thread runs none.
pub(crate) fn sync(fd: &Lease, data_only: bool) -> Result<SlotJob<io::Result<()>>, ()> {
    submit(
        fd,
        0,
        data_only,
        |data_only| FileMem::Sync { data_only },
        as_sync,
    )
    .map_err(drop)
}

/// A change of the space the `len` bytes from `offset` use, as `mode` says,
/// on this thread's ring; `Err` when the thread runs none.
pub(crate) fn allocate(
    fd: &Lease,
    offset: u64,
    len: u64,
    mode: Allocate,
) -> Result<SlotJob<io::Result<()>>, ()> {
    submit(
        fd,
        offset,
        (len, mode),
        |(len, mode)| FileMem::Allocate { len, mode },
        as_write,
    )
    .map_err(drop)
}

/// A write-back of the `len` bytes from `offset` (zero: to the end), on this
/// thread's ring; `Err` when the thread runs none.
pub(crate) fn sync_range(fd: &Lease, offset: u64, len: u32) -> Result<SlotJob<io::Result<()>>, ()> {
    submit(fd, offset, len, |len| FileMem::SyncRange { len }, as_sync).map_err(drop)
}

/// A read at `at` into the spare capacity of a task's `buf`.
pub(crate) fn read_lent(
    fd: &Lease,
    at: u64,
    buf: OwnedBuf,
) -> Result<SlotJob<LentReadEnd>, OwnedBuf> {
    submit(fd, at, buf, FileMem::ReadLent, as_read_lent)
}

/// A write of all of `buf` at `at`.
pub(crate) fn write(fd: &Lease, at: u64, buf: PoolBuf) -> Result<SlotJob<io::Result<()>>, PoolBuf> {
    submit(fd, at, buf, |buf| FileMem::Write { buf, done: 0 }, as_write)
}

/// A write of all of a task's `buf` at `at`.
pub(crate) fn write_lent(
    fd: &Lease,
    at: u64,
    buf: OwnedBuf,
) -> Result<SlotJob<LentWriteEnd>, OwnedBuf> {
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
        let fd = self.fd.raw();
        match &mut self.mem {
            FileMem::Read { buf, from, len } => {
                let spare = buf.spare();
                let room = spare.get_mut(*from as usize..).unwrap_or_default();
                let len = (*len as usize).min(room.len());
                Sqe::read(fd, room.as_mut_ptr(), len32(len), self.at)
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
                let bytes = buf.rest_from(*done);
                Sqe::write(
                    fd,
                    bytes.as_ptr(),
                    len32(bytes.len()),
                    self.at + *done as u64,
                )
            }
            FileMem::Sync { data_only } => Sqe::fsync(fd, *data_only),
            FileMem::Allocate { len, mode } => {
                Sqe::fallocate(fd, self.at, *len, sys::fallocate_mode(*mode) as u32)
            }
            FileMem::SyncRange { len } => Sqe::sync_file_range(fd, self.at, *len, SYNC_RANGE_FLAGS),
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
            FileMem::Read { mut buf, from, .. } => FileOutcome::Read(result.map(|n| {
                buf.set_filled_at(from as usize, n);
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
            FileMem::Sync { .. } | FileMem::SyncRange { .. } => FileOutcome::Sync(result.map(drop)),
            FileMem::Allocate { .. } => FileOutcome::Write(result.map(drop)),
        };
        self.slot.put(outcome);
        // Once the wake went: a task it dropped handed what it left first.
        drop(self.write.take());
        None
    }

    /// The operation is kept for good, its completion never to come: a
    /// runtime's drop does not wait for it.
    pub(crate) fn give_up(&mut self) {
        self.write = None;
    }

    /// Makes what is left of the call on the blocking pool of the runtime
    /// this thread is inside, and leaves its end in the slot. With none (a
    /// ring's keeper, itself a blocking job or a thread of its own), it is
    /// made here: no runtime is started for it.
    fn finish_on_pool(self) {
        let FileOp {
            fd,
            at,
            mem,
            slot,
            write,
        } = self;
        let call = move || {
            let raw = fd.raw();
            let outcome = match mem {
                FileMem::Read { mut buf, from, len } => {
                    let (from, len) = (from as usize, len as usize);
                    let spare = buf.spare();
                    let end = from.saturating_add(len).min(spare.len());
                    let room = spare.get_mut(from..end).unwrap_or_default();
                    FileOutcome::Read(sys::pread(raw, room, at).map(|n| {
                        buf.set_filled_at(from, n);
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
                    let result = crate::fs::file::pwrite_owned(raw, &buf, done, at + done as u64);
                    FileOutcome::WriteLent(result, buf)
                }
                FileMem::Sync { data_only } => FileOutcome::Sync(sync_on(
                    &fd,
                    if data_only {
                        SyncKind::Data
                    } else {
                        SyncKind::All
                    },
                )),
                FileMem::SyncRange { len } => FileOutcome::Sync(sync_on(
                    &fd,
                    SyncKind::Range {
                        offset: at,
                        len: u64::from(len),
                    },
                )),
                FileMem::Allocate { len, mode } => {
                    FileOutcome::Write(sys::allocate(raw, at, len, mode))
                }
            };
            slot.put(outcome);
            // Once the wake went: a task it dropped handed what it left first.
            drop(write);
        };
        match crate::rt::runtime::blocking_pool_inside() {
            Some(pool) => pool.spawn(call).detach(),
            None => call(),
        }
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};
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

    /// An operation on `file`, through a lease of a source over its own
    /// descriptor (the file outlives the operation).
    fn op_of(file: &std::fs::File, at: u64, mem: FileMem) -> (FileOp, Arc<Slot>) {
        use std::os::fd::AsRawFd;
        let slot = Slot::new();
        let source = crate::io::Source::new(file.as_raw_fd(), crate::io::source::Kind::File);
        (
            FileOp {
                fd: source.lease().unwrap(),
                at,
                mem,
                slot: Arc::clone(&slot),
                write: None,
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
                buf: OwnedBuf::Vec(b"0123456789".to_vec()),
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
            Some(FileOutcome::WriteLent(Ok(()), buf)) => assert_eq!(&*buf, b"0123456789"),
            _ => panic!("the write did not end whole"),
        }
        std::fs::remove_file(path).unwrap();
    }

    /// A call cancelled because its shard went is made on the blocking pool
    /// (here, on a thread inside no runtime): a write still lands, a read
    /// still reads.
    #[test]
    fn a_call_cancelled_as_its_shard_went_is_made_on_the_pool() {
        let (mut file, path) = temp_file("cancelled");
        let (op, slot) = op_of(
            &file,
            0,
            FileMem::WriteLent {
                buf: OwnedBuf::Vec(b"landed".to_vec()),
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
