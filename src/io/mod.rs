//! IO: [`Handle`] (async operations on std's sockets and files), the
//! `futures-io` traits, and the utilities every stream gets: the reads and
//! writes of [`AsyncReadExt`], [`AsyncWriteExt`], [`AsyncBufReadExt`] and
//! [`AsyncSeekExt`] as futures, [`ReadBuf`], [`copy`] and
//! [`copy_bidirectional`], [`BufReader`], [`BufWriter`] and [`BufStream`],
//! the in-memory pipes [`duplex`] and [`simplex`], [`split`], [`empty`],
//! [`repeat`] and [`sink`], the process's [`stdin`], [`stdout`] and
//! [`stderr`], what the sockets' readiness calls take and answer
//! ([`Interest`], [`Ready`]), and on unix [`AsyncFd`](unix::AsyncFd), a
//! caller's own descriptor driven by readiness.
//!
//! Inside, the shards' half of IO: sources, the drivers' half of every
//! handle, and the system calls they make. A handle's IO object stays its own
//! (std's sockets and files); its source is what the shard's driver reaches.
//! The driver completes every operation, into and out of the pool's buffers,
//! at once when it can and on readiness otherwise, and wakes the task, which
//! takes the result.

#[cfg(unix)]
mod async_fd;
#[cfg(unix)]
pub(crate) mod caller;
mod handle;
pub(crate) mod raw;
mod ready;
pub(crate) mod side;
pub(crate) mod source;
pub(crate) mod sys;
mod util;

pub use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

pub use self::handle::{Handle, IoObject};
pub use self::ready::{Interest, Ready};

/// Unix's own: [`AsyncFd`](unix::AsyncFd), a caller's descriptor driven by
/// readiness, and its guards.
#[cfg(unix)]
pub mod unix {
    pub use super::async_fd::{AsyncFd, AsyncFdReadyGuard, AsyncFdReadyMutGuard, TryIoError};
}
pub(crate) use self::source::Source;
pub use self::util::{
    AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufStream, BufWriter,
    Chain, DuplexStream, Empty, FillBuf, Flush, Lines, Read, ReadBuf, ReadBufFuture, ReadExact,
    ReadHalf, ReadLine, ReadNumber, ReadToEnd, ReadToString, ReadUntil, Repeat, Seek, Shutdown,
    SimplexReader, SimplexWriter, Sink, Split, Stderr, Stdin, Stdout, Take, Write, WriteAll,
    WriteHalf, WriteNumber, WriteVectored, copy, copy_bidirectional, copy_bidirectional_with_sizes,
    copy_buf, duplex, empty, repeat, simplex, sink, split, stderr, stdin, stdout,
};
