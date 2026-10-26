//! The IO utilities: extension traits with the reads and writes every
//! stream gets as futures, a read buffer over uninitialized memory, copying,
//! buffering, in-memory pipes, and readers and writers with nothing behind
//! them. All of it works on any `futures-io` stream, nuclei's or not.

mod buf_read_ext;
mod buffered;
mod copy;
mod duplex;
mod misc;
mod read_buf;
mod read_ext;
mod split;
mod stdio;
mod write_ext;

pub use self::buf_read_ext::{
    AsyncBufReadExt, AsyncSeekExt, FillBuf, Lines, ReadLine, ReadUntil, Seek, Split,
};
pub use self::buffered::{BufReader, BufStream, BufWriter};
pub use self::copy::{copy, copy_bidirectional, copy_bidirectional_with_sizes, copy_buf};
pub use self::duplex::{DuplexStream, SimplexReader, SimplexWriter, duplex, simplex};
pub use self::misc::{Empty, Repeat, Sink, empty, repeat, sink};
pub use self::read_buf::ReadBuf;
pub use self::read_ext::{
    AsyncReadExt, Chain, Read, ReadBufFuture, ReadExact, ReadNumber, ReadToEnd, ReadToString, Take,
};
pub use self::split::{ReadHalf, WriteHalf, split};
pub use self::stdio::{Stderr, Stdin, Stdout, stderr, stdin, stdout};
pub use self::write_ext::{
    AsyncWriteExt, Flush, Shutdown, Write, WriteAll, WriteNumber, WriteVectored,
};
