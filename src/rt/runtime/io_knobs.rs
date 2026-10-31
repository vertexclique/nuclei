//! The builder's knobs of a runtime's IO handles: how long a dropped handle's bytes written
//! behind may take to reach the kernel, the least a zero-copy write must have left for a send
//! of it to go zero-copy, and the most one call of a `send_file` sends. Each is read once, as
//! the runtime is built: the handles read the runtime's copy where their work runs, and a
//! worker's ring keeps its own copy of the threshold.

use std::io;
use std::time::Duration;

use super::Builder;

/// The most one call that sends a file may be asked to send: what Linux's `sendfile(2)` sends in
/// one call (`0x7ffff000` bytes), which is less than Windows' `TransmitFile` takes.
const SEND_FILE_CHUNK_MAX: usize = 0x7fff_f000;

/// What a runtime's handles read of its builder.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IoKnobs {
    /// How long a dropped handle's bytes written behind may take to reach the kernel.
    pub(crate) linger: Duration,
    /// The most bytes one call of a `send_file` sends.
    pub(crate) send_file_chunk: u64,
}

impl Default for IoKnobs {
    fn default() -> Self {
        Self {
            linger: crate::io::handle::LINGER,
            send_file_chunk: crate::net::send_file::SEND_FILE_CHUNK,
        }
    }
}

/// The builder's own settings of [`IoKnobs`]; an unset one takes its default.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct IoSettings {
    linger: Option<Duration>,
    send_file_chunk: Option<usize>,
}

impl IoSettings {
    /// Refuses a setting no handle can work with, as the configuration's mistake.
    pub(super) fn validate(&self) -> io::Result<()> {
        use crate::config::refuse;
        if let Some(linger) = self.linger.filter(|l| l.is_zero()) {
            return Err(refuse(
                "write_behind_linger",
                format_args!("{linger:?}"),
                "more than zero",
            ));
        }
        if let Some(bytes) = self
            .send_file_chunk
            .filter(|&b| !(1..=SEND_FILE_CHUNK_MAX).contains(&b))
        {
            return Err(refuse(
                "send_file_chunk",
                bytes,
                format_args!("from 1 to {SEND_FILE_CHUNK_MAX} bytes"),
            ));
        }
        Ok(())
    }

    /// The knobs, each set one as set and the rest at their defaults.
    pub(super) fn resolve(self) -> IoKnobs {
        let default = IoKnobs::default();
        IoKnobs {
            linger: self.linger.unwrap_or(default.linger),
            send_file_chunk: self
                .send_file_chunk
                .map_or(default.send_file_chunk, |b| b as u64),
        }
    }
}

impl Builder {
    /// How long the bytes a dropped handle wrote behind may take to reach the kernel (default
    /// one minute): a task delivers them after the drop, as the kernel delivers what a closed
    /// socket holds. Past it the rest is given up, and a TCP stream's connection is reset, so its
    /// peer reads an error rather than the end of a stream cut short. [`get_mut`] and
    /// [`into_inner`] wait for the bytes as long at most, and the runtime's drop waits for the
    /// tasks. More than zero; [`build`](Self::build) fails with `InvalidInput` at zero.
    ///
    /// [`get_mut`]: crate::io::Handle::get_mut
    /// [`into_inner`]: crate::io::Handle::into_inner
    pub fn write_behind_linger(mut self, linger: Duration) -> Self {
        self.io.linger = Some(linger);
        self
    }

    /// The least a [`write_zero_copy`](crate::net::TcpStream::write_zero_copy) must have left
    /// to send for its next send to go zero-copy, in bytes (default 1 MiB): below it, the kernel
    /// copies the bytes, as it does for every write. Copying costs less than pinning the pages
    /// for small writes; where the threshold pays depends on the machine and its network. Only
    /// io_uring sends zero-copy, on the runtime's workers: every value is accepted, and `0`
    /// sends every write zero-copy, `usize::MAX` none.
    #[cfg_attr(not(ring), expect(unused_mut, reason = "only a ring sends zero-copy"))]
    pub fn zero_copy_threshold(mut self, bytes: usize) -> Self {
        #[cfg(ring)]
        {
            self.tuning.zero_copy_min = bytes;
        }
        #[cfg(not(ring))]
        let _ = bytes;
        self
    }

    /// The most bytes one call of a [`send_file`](crate::net::TcpStream::send_file) sends
    /// (default 1 MiB). Each call runs on the blocking pool and may stall on a cold page there,
    /// so this bounds how long a pool thread is held, and how much a cancelled `send_file` still
    /// sends after its future went; where the system has no call for it, each chunk is read
    /// into a buffer of this size. From 1 byte to `0x7ffff000` (what one `sendfile(2)` sends);
    /// [`build`](Self::build) fails with `InvalidInput` outside.
    pub fn send_file_chunk(mut self, bytes: usize) -> Self {
        self.io.send_file_chunk = Some(bytes);
        self
    }
}

/// The knobs of the runtime this thread is inside, or of the default runtime if it runs; their
/// defaults where neither does.
pub(crate) fn current() -> IoKnobs {
    super::running_inner().map_or_else(IoKnobs::default, |inner| inner.io)
}

#[cfg(test)]
mod tests;
