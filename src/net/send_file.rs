//! `send_file`: a range of a file sent to a stream.
//!
//! Where the system has a call for it (`sendfile(2)`, or `TransmitFile` on
//! Windows), the bytes go from the page cache to the socket in the kernel
//! and never enter user space; the
//! call reads the file, so a page that is not cached blocks the thread that
//! makes it, and it is made on the blocking pool, a chunk at a time, never
//! on a shard. A full socket is waited for through the stream's own
//! readiness. Where the system has no such call, the range is read at its
//! offset on the blocking pool and written with `write_owned`.
//!
//! Why the blocking pool rather than the nonblocking flags: `sendfile(2)`
//! takes no flag that keeps it off the disk, and a nonblocking socket only
//! stops it at the socket.

use std::io;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use crate::fs::File;
use crate::io::Handle;
#[cfg(unix)]
use crate::io::Interest;

/// The most one call of the blocking pool sends: what a stall on a cold
/// page holds a pool thread for is bounded, and so is a cancelled call's
/// bytes that still land.
const SEND_FILE_CHUNK: u64 = 1 << 20;

/// A range of `file` that `len` bytes from `offset` name, or the error of a
/// range no file can have.
fn range(offset: u64, len: u64) -> io::Result<u64> {
    offset.checked_add(len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the range of the file runs past what an offset can name",
        )
    })
}

/// Reads up to `buf.len()` bytes of `file` at `offset`, without moving any
/// cursor.
fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_at(file, buf, offset);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(file, buf, offset);
}

macro_rules! send_file {
    ($($ty:ty),* $(,)?) => {$(
        impl Handle<$ty> {
            /// Sends up to `len` bytes of `file`, from `offset`, to the
            /// stream, and returns how many it sent: fewer than `len` when
            /// the file ends first.
            ///
            /// Where the system has `sendfile(2)` (Linux, Android, macOS,
            /// FreeBSD) or `TransmitFile` (Windows, through the completion
            /// port) the bytes go from the page cache to the socket in the
            /// kernel and never enter user space; elsewhere (and on a Windows
            /// provider without `TransmitFile`) they are read at `offset` and
            /// written, which works the same and copies. The
            /// file's own cursor does not move. Bytes written to the stream
            /// before the call go out first; do not write to the stream
            /// while the call runs, or its bytes may land between the
            /// call's.
            ///
            /// If the future is dropped before it completes, the stream has
            /// got a prefix of the range, in order and whole, and a chunk of
            /// at most one MiB may still be on its way.
            ///
            /// # Errors
            ///
            /// An error the stream or the file met (also the error of an
            /// earlier write behind), `InvalidInput` for a range past what
            /// an offset can name, and `file` not being a regular file.
            /// The bytes sent before an error are not counted.
            pub async fn send_file(&self, file: &File, offset: u64, len: u64) -> io::Result<u64> {
                range(offset, len)?;
                core::future::poll_fn(|cx| self.source().poll_flush(cx)).await?;
                if len == 0 {
                    return Ok(0);
                }
                #[cfg(unix)]
                if crate::io::sys::SENDS_FILES {
                    return self.send_file_in_kernel(file, offset, len).await;
                }
                #[cfg(windows)]
                if crate::io::sys::transmit_file_available() {
                    return self.send_file_transmitted(file, offset, len).await;
                }
                self.send_file_by_copy(file, offset, len).await
            }

            #[cfg(unix)]
            async fn send_file_in_kernel(&self, file: &File, offset: u64, len: u64) -> io::Result<u64> {
                let sock = Arc::new(crate::io::sys::duplicate(self.get_ref().as_raw_fd())?);
                let from = Arc::new(crate::io::sys::duplicate(file.get_ref().as_raw_fd())?);
                let mut sent = 0u64;
                while sent < len {
                    let want = (len - sent).min(SEND_FILE_CHUNK) as usize;
                    let at = offset + sent;
                    let (out, src) = (Arc::clone(&sock), Arc::clone(&from));
                    // The socket's `WouldBlock` is an answer, not a failure
                    // to count: it rides out inside the `Ok`.
                    let step = crate::rt::blocking::call(move || {
                        Ok(crate::io::sys::send_file(out.as_raw_fd(), src.as_raw_fd(), at, want))
                    })
                    .await?;
                    match step {
                        Ok(0) => break,
                        Ok(n) => sent += n as u64,
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            let raw = sock.as_raw_fd();
                            self.async_io(Interest::WRITABLE, || crate::io::sys::poll_writable(raw))
                                .await?;
                        }
                        Err(err) => {
                            crate::rt::counters::io_error(&err);
                            return Err(err);
                        }
                    }
                }
                Ok(sent)
            }

            /// `TransmitFile` on the completion port, a chunk at a time. The range is cut to
            /// the file first, as the call sends exactly what it is asked for or fails.
            #[cfg(windows)]
            async fn send_file_transmitted(&self, file: &File, offset: u64, len: u64) -> io::Result<u64> {
                use std::os::windows::io::AsRawHandle;
                let raw = file.get_ref().as_raw_handle() as usize as crate::io::raw::Raw;
                // A file whose sharing forbids another reader is sent by copy, which reads at
                // the offsets through the handle itself.
                let Ok(handle) = crate::io::sys::reopen_overlapped(raw) else {
                    return self.send_file_by_copy(file, offset, len).await;
                };
                let handle = Arc::new(handle);
                let len = len.min(file.get_ref().metadata()?.len().saturating_sub(offset));
                let mut sent = 0u64;
                while sent < len {
                    let want = (len - sent).min(SEND_FILE_CHUNK) as u32;
                    let wrote = self
                        .source()
                        .send_file_range(Arc::clone(&handle), offset + sent, want)
                        .await?;
                    sent += wrote as u64;
                }
                Ok(sent)
            }

            async fn send_file_by_copy(&self, file: &File, offset: u64, len: u64) -> io::Result<u64> {
                let from = Arc::new(file.get_ref().try_clone()?);
                let mut buf = vec![0u8; SEND_FILE_CHUNK.min(len) as usize];
                let mut sent = 0u64;
                while sent < len {
                    let want = (len - sent).min(buf.len() as u64) as usize;
                    let at = offset + sent;
                    let from = Arc::clone(&from);
                    let (read, back) = crate::rt::blocking::call(move || {
                        let read = read_at(&from, &mut buf[..want], at);
                        Ok((read, buf))
                    })
                    .await?;
                    buf = back;
                    let n = read?;
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    let (wrote, back) = self.write_owned(buf).await;
                    buf = back;
                    wrote?;
                    buf.resize(SEND_FILE_CHUNK.min(len) as usize, 0);
                    sent += n as u64;
                }
                Ok(sent)
            }
        }
    )*};
}

send_file!(TcpStream);
#[cfg(unix)]
send_file!(UnixStream);
