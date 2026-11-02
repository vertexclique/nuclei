//! Advisory locks on a whole file, without blocking a thread.
//!
//! `lock_file` never waits in the kernel and never takes a thread of the
//! blocking pool. It makes the call that never waits (`try_lock_file`'s) on the
//! calling thread, and while another holder has the lock the future sleeps on a
//! timer and asks again. A future dropped while it sleeps holds nothing, since
//! the ask and the return of the lock are one step.
//! `proofs/tla/flock/FileLock.tla` checks the protocol.

use core::future::poll_fn;
use core::pin::Pin;
use core::time::Duration;
use std::fs::{File, TryLockError};
use std::io;

use futures_io::AsyncWrite;

use crate::io::Handle;

/// The pause before the first new ask for a lock another holder has.
const FIRST: Duration = Duration::from_micros(50);
/// The longest pause between two asks.
const LAST: Duration = Duration::from_millis(20);

impl Handle<File> {
    /// Locks the whole file, shared for any number of holders if `shared`,
    /// else exclusively for one, as std's `File::lock_shared` and
    /// `File::lock` do. The lock is advisory, and it is released by
    /// [`unlock_file`](Self::unlock_file) or when the file is closed.
    ///
    /// The call never waits in the kernel and never takes a thread of the
    /// blocking pool. While another holder has the lock the future sleeps on a
    /// timer, from 50 microseconds doubling to 20 milliseconds, and asks again,
    /// so a lock that comes free is taken within that pause. It takes no place
    /// in the kernel's line of waiters: a holder that takes the lock again at
    /// once may keep it from this one. A future dropped while it waits holds
    /// nothing. See `proofs/tla/flock/FileLock.tla`.
    ///
    /// std's own `File::lock`, reached through `Deref`, blocks the worker
    /// thread that makes it.
    ///
    /// # Errors
    ///
    /// The system's own, as std's.
    pub async fn lock_file(&self, shared: bool) -> io::Result<()> {
        let mut pause = FIRST;
        loop {
            match self.try_lock_once(shared) {
                Ok(()) => return Ok(()),
                Err(TryLockError::Error(err)) => return Err(err),
                Err(TryLockError::WouldBlock) => {
                    crate::time::sleep(pause).await;
                    pause = (pause * 2).min(LAST);
                }
            }
        }
    }

    /// The one call that never waits, for `lock_file` and `try_lock_file`.
    fn try_lock_once(&self, shared: bool) -> Result<(), TryLockError> {
        let lease = self.source().lease().map_err(TryLockError::Error)?;
        let file = lease.file();
        if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        }
    }

    /// Locks the whole file if it is free now, shared if `shared`, else
    /// exclusively, as std's `File::try_lock_shared` and `File::try_lock` do.
    /// The call does not wait, so it is made on the calling thread.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when another holder has the lock, else the system's own
    /// error, as std's.
    pub async fn try_lock_file(&self, shared: bool) -> Result<(), TryLockError> {
        self.try_lock_once(shared)
    }

    /// Releases the lock this file holds, as std's `File::unlock` does, once
    /// every write accepted before the call landed (as a flush waits for
    /// them), so a holder that wrote under the lock lets go only after its
    /// bytes are in the file.
    ///
    /// # Errors
    ///
    /// A write that failed (reported here as by a flush), or the system's own
    /// error. The lock is released even when a write failed.
    pub async fn unlock_file(&self) -> io::Result<()> {
        let mut this = self;
        let flushed = poll_fn(|cx| Pin::new(&mut this).poll_flush(cx)).await;
        let unlocked = self
            .source()
            .lease()
            .and_then(|lease| lease.file().unlock());
        flushed.and(unlocked)
    }
}
