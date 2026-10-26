//! IO: [`Handle`] (async operations on std's sockets and files), the
//! `futures-io` traits, and the utilities every stream gets: the reads and
//! writes of [`AsyncReadExt`], [`AsyncWriteExt`], [`AsyncBufReadExt`] and
//! [`AsyncSeekExt`] as futures, [`ReadBuf`], [`copy`] and
//! [`copy_bidirectional`], [`BufReader`], [`BufWriter`] and [`BufStream`],
//! the in-memory pipes [`duplex`] and [`simplex`], [`split`], and [`empty`],
//! [`repeat`] and [`sink`].
//!
//! Inside, the shards' half of IO: sources, the drivers' half of every
//! handle, and the system calls they make. A handle's IO object stays its own
//! (std's sockets and files); its source is what the shard's driver reaches.
//! The driver completes every operation, into and out of the pool's buffers,
//! at once when it can and on readiness otherwise, and wakes the task, which
//! takes the result.

mod handle;
pub(crate) mod raw;
pub(crate) mod side;
pub(crate) mod source;
pub(crate) mod sys;
mod util;

pub use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

pub use self::handle::{Handle, IoObject};
pub(crate) use self::source::Source;
pub use self::util::*;
