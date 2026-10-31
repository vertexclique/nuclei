//! Advisory locks on a whole file, without blocking the worker.
//!
//! `lock` waits in the kernel until the file is free, so it is a job of the
//! blocking pool. A future dropped while the job waits must not leave the file
//! locked with nobody to give it back, so the future and the job share one
//! state word per call: WAITING, HELD or GONE. After the job got the lock it
//! moves the word from WAITING to HELD, and if it finds GONE it unlocks the
//! file itself. A future dropped before it returned its result swaps GONE in,
//! and if the word was HELD it unlocks the file; then its job, if it has not
//! started, is cancelled and never locks.
//! `proofs/tla/flock/FileLock.tla` checks the protocol.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{Context, Poll};
use std::fs::{File, TryLockError};
use std::io;
use std::sync::Arc;

use futures_io::AsyncWrite;

use crate::io::Handle;
use crate::io::source::requests::Lease;
use crate::rt::task::Join;

/// The job has not got the lock yet.
const WAITING: u8 = 0;
/// The job got the lock and its future is still there to take it.
const HELD: u8 = 1;
/// The future was dropped before it took the result.
const GONE: u8 = 2;

/// A `lock` call: the future of the pool's job, and what it must give back.
struct LockCall {
    state: Arc<AtomicU8>,
    lease: Lease,
    job: Join<io::Result<()>>,
    /// The result was taken: the lock is the caller's.
    settled: bool,
}

impl Future for LockCall {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ended = core::task::ready!(Pin::new(&mut self.job).poll(cx));
        self.settled = true;
        Poll::Ready(ended.unwrap_or_else(|lost| Err(lost.into())))
    }
}

impl Drop for LockCall {
    fn drop(&mut self) {
        if !self.settled && self.state.swap(GONE, Ordering::AcqRel) == HELD {
            let _ = self.lease.file().unlock();
        }
    }
}

impl Handle<File> {
    /// Locks the whole file, shared for any number of holders if `shared`,
    /// else exclusively for one, as std's `File::lock_shared` and
    /// `File::lock` do, waiting for the lock on the blocking pool so the
    /// worker is free meanwhile. The lock is advisory, and it is released by
    /// [`unlock`](Self::unlock) or when the file is closed.
    ///
    /// If the future is dropped while it waits, the lock is never left held
    /// with nobody to release it: a job that had not started is cancelled,
    /// and one that got the lock gives it back.
    ///
    /// This name shadows std's `File::lock`, which is reached through `Deref`
    /// and blocks the worker thread; `(*file).lock()` and
    /// `file.get_ref().lock()` still reach std's.
    ///
    /// # Errors
    ///
    /// The system's own, as std's.
    pub async fn lock(&self, shared: bool) -> io::Result<()> {
        let lease = self.source().lease()?;
        let state = Arc::new(AtomicU8::new(WAITING));
        let (job_lease, job_state) = (lease.clone(), Arc::clone(&state));
        let job = crate::runtime::spawn_blocking(move || {
            let file = job_lease.file();
            let locked = if shared {
                file.lock_shared()
            } else {
                file.lock()
            };
            if locked.is_ok()
                && job_state
                    .compare_exchange(WAITING, HELD, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                let _ = file.unlock();
            }
            locked
        })
        .join();
        LockCall {
            state,
            lease,
            job,
            settled: false,
        }
        .await
    }

    /// Locks the whole file if it is free now, shared if `shared`, else
    /// exclusively, as std's `File::try_lock_shared` and `File::try_lock` do.
    /// The call does not wait, so it is made on the calling thread.
    ///
    /// This name shadows std's `File::try_lock`, which is reached through
    /// `Deref`; `(*file).try_lock()` still reaches it.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when another holder has the lock, else the system's own
    /// error, as std's.
    pub async fn try_lock(&self, shared: bool) -> Result<(), TryLockError> {
        let lease = self.source().lease().map_err(TryLockError::Error)?;
        let file = lease.file();
        if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        }
    }

    /// Releases the lock this file holds, as std's `File::unlock` does, once
    /// every write accepted before the call landed (as a flush waits for
    /// them), so a holder that wrote under the lock lets go only after its
    /// bytes are in the file.
    ///
    /// This name shadows std's `File::unlock`, which is reached through
    /// `Deref`; `(*file).unlock()` still reaches it.
    ///
    /// # Errors
    ///
    /// A write that failed (reported here as by a flush), or the system's own
    /// error. The lock is released even when a write failed.
    pub async fn unlock(&self) -> io::Result<()> {
        let mut this = self;
        let flushed = poll_fn(|cx| Pin::new(&mut this).poll_flush(cx)).await;
        let unlocked = self
            .source()
            .lease()
            .and_then(|lease| lease.file().unlock());
        flushed.and(unlocked)
    }
}
