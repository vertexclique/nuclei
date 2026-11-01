//! A read at the cursor the page cache answers at once, made on the task's
//! own thread with no job: `preadv2` with `RWF_NOWAIT` (Linux 4.14), for
//! which the kernel never waits for the device or a lock, answering `EAGAIN`
//! instead. A read it cannot answer so goes to its job, on the ring or the
//! blocking pool, as every read did before; so does the next one, and the one
//! after that tries the cache again once that job's bytes were taken (the
//! job's read brought what follows into the cache as it read ahead).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::buf::owned::OwnedBuf;
use crate::io::raw::Raw;

/// Whether the kernel has the call: cleared for good if it has none.
static CALL: AtomicBool = AtomicBool::new(true);

/// Whether the kernel has the call, as far as a read found.
#[cfg(test)]
pub(super) fn has_call() -> bool {
    CALL.load(Ordering::Relaxed)
}

/// Whether a file's reads at the cursor try the page cache first.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum Cache {
    /// The next read tries it.
    #[default]
    Try,
    /// The last one found its bytes missing: reads go to their jobs until
    /// a job's bytes are taken.
    Cold,
    /// The file's filesystem refused the flag: every read goes to its job.
    Never,
}

/// What a read the page cache may answer did.
pub(super) enum Now {
    /// It read `.0` bytes (none at the file's end) into the buffer's spare
    /// capacity.
    Read(usize),
    /// The cache did not have them: the read goes to its job.
    Missed,
    /// The read goes to its job: the call, or this file, takes no such
    /// read, or the read is of another shape (a vectored one).
    Job,
}

/// Reads at `at` on `fd` into the spare capacity of `buf`, if the page cache
/// has the bytes now.
#[inline]
pub(super) fn read(fd: Raw, buf: &mut OwnedBuf, at: u64, cache: &mut Cache) -> Now {
    if *cache != Cache::Try || !CALL.load(Ordering::Relaxed) || buf.vectored_mut().is_some() {
        return Now::Job;
    }
    let Ok(offset) = i64::try_from(at) else {
        return Now::Job;
    };
    let spare = buf.spare_capacity_mut();
    #[cfg(test)]
    crate::io::sys::file_calls::note(fd, (b'r', spare.len(), at));
    let iov = libc::iovec {
        iov_base: spare.as_mut_ptr().cast(),
        iov_len: spare.len(),
    };
    // SAFETY: the kernel writes at most `iov_len` bytes into the spare
    // capacity `iov` names, which outlives the call; the offset is the low
    // half, the high one naught on a 64-bit kernel.
    let res = unsafe {
        crate::io::sys::syscall(
            libc::SYS_preadv2,
            [
                fd as usize,
                (&iov as *const libc::iovec) as usize,
                1,
                offset as usize,
                0,
                libc::RWF_NOWAIT as usize,
            ],
        )
    };
    match res {
        Ok(n) => {
            // SAFETY: the kernel wrote the first `n` bytes of the spare
            // capacity.
            unsafe { buf.add_len(n) };
            Now::Read(n)
        }
        Err(err) => match err.raw_os_error() {
            Some(libc::EAGAIN) => {
                *cache = Cache::Cold;
                Now::Missed
            }
            Some(libc::ENOSYS) => {
                CALL.store(false, Ordering::Relaxed);
                Now::Job
            }
            Some(libc::EOPNOTSUPP | libc::EINVAL) => {
                *cache = Cache::Never;
                Now::Job
            }
            // Any other answer is the job's to give.
            _ => Now::Job,
        },
    }
}
