use std::{io, task};
use std::{task::Poll, fs::File, pin::Pin, task::Context};
use super::handle::Handle;
use futures::io::{AsyncRead, AsyncWrite, SeekFrom};
use super::submission_handler::SubmissionHandler;
use std::io::Read;

use std::net::{TcpStream};

#[cfg(unix)]
use std::{mem::ManuallyDrop, os::unix::io::{AsRawFd, RawFd, FromRawFd}, os::unix::prelude::*};
#[cfg(unix)]
use std::os::unix::net::{UnixStream};

use std::future::Future;

use crate::syscore::Processor;
use crate::syscore::*;
use std::sync::Arc;
use futures::{AsyncBufRead, AsyncSeek};
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use futures_util::pending_once;
use lever::prelude::TTas;
use crate::Proactor;
use std::path::Path;

//
// Proxy operations for Future registration via AsyncRead, AsyncWrite and others.
// Linux, windows etc. specific

macro_rules! impl_async_read {
    ($name:ident) => {
        impl AsyncRead for Handle<$name> {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_read(cx, buf)
            }
        }
    }
}

macro_rules! impl_async_write {
    ($name:ident) => {
        impl AsyncWrite for Handle<$name> {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_write(cx, buf)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_close(cx)
            }
        }
    }
}

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl_async_read!(File);
#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl_async_write!(File);

impl_async_read!(TcpStream);
impl_async_write!(TcpStream);

#[cfg(unix)]
impl_async_read!(UnixStream);
#[cfg(unix)]
impl_async_write!(UnixStream);


///////////////////////////////////
///// Non proactive File
///////////////////////////////////

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl AsyncRead for &Handle<File> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_read_file(&file, buf).await?;

            let _ = ManuallyDrop::new(file);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl AsyncWrite for &Handle<File> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_write_file(&file, buf).await?;

            let _ = ManuallyDrop::new(file);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}


///////////////////////////////////
///// IO URING / Proactive / Linux
///////////////////////////////////

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncRead for Handle<File> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut inner = futures::ready!(self.as_mut().poll_fill_buf(cx))?;
        let len = io::Read::read(&mut inner, buf)?;
        self.consume(len);
        println!("LEN – {}", len);
        Poll::Ready(Ok(len))
    }
}

#[cfg(all(feature = "iouring", target_os = "linux"))]
const NON_READ: &[u8] = &[];

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncBufRead for Handle<File> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let fd = self.as_raw_fd();
        let mut store = &mut self.get_mut().store_file;

        if let Some(mut store_file) = store.as_mut() {
            let fd = store_file.get_fd();
            let op_state = store_file.op_state();
            let (bufp, pos) = store_file.bufpair();

            println!("Read: {}", *pos);

            let filled_buf = bufp.fill_buf(|buf| Handle::<File>::filler(cx, fd, pos, buf));
            match filled_buf {
                Poll::Ready(Err(_)) => {
                    dbg!(&filled_buf);
                },
                _ => {}
            }
            filled_buf
        } else {
            // Poll::Pending
            dbg!("NON READ");
            Poll::Ready(Ok(NON_READ))
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        dbg!(amt);
        let mut store = self.get_mut().store_file.as_mut().unwrap();
        store.buf().consume(amt);
    }
}


#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncWrite for &Handle<File> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncSeek for Handle<File> {
    fn poll_seek(self: Pin<&mut Self>, cx: &mut Context<'_>, pos: SeekFrom) -> Poll<io::Result<u64>> {
        let mut store = &mut self.get_mut().store_file.as_mut().unwrap();

        let (whence, offset) = match pos {
            io::SeekFrom::Start(n) => {
                *store.pos() = n as usize;
                return Poll::Ready(Ok(*store.pos() as u64));
            }
            io::SeekFrom::Current(n) => (*store.pos(), n),
            io::SeekFrom::End(n)     => {
                let fut = store.poll_file_size();
                futures::pin_mut!(fut);
                (futures::ready!(fut.as_mut().poll(cx))?, n)
            }
        };
        let valid_seek = if offset.is_negative() {
            match whence.checked_sub(offset.abs() as usize) {
                Some(valid_seek) => valid_seek,
                None => {
                    let invalid = io::Error::from(io::ErrorKind::InvalidInput);
                    return Poll::Ready(Err(invalid));
                }
            }
        } else {
            match whence.checked_add(offset as usize) {
                Some(valid_seek) => valid_seek,
                None => {
                    let overflow = io::Error::from_raw_os_error(libc::EOVERFLOW);
                    return Poll::Ready(Err(overflow));
                }
            }
        };
        *store.pos() = valid_seek;
        Poll::Ready(Ok(*store.pos() as u64))
    }
}

///////////////////////////////////
///// TcpStream
///////////////////////////////////

#[cfg(unix)]
impl AsyncRead for &Handle<TcpStream> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_recv(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(unix)]
impl AsyncWrite for &Handle<TcpStream> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_send(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}


#[cfg(unix)]
impl AsyncRead for &Handle<UnixStream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { UnixStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_recv(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(unix)]
impl AsyncWrite for &Handle<UnixStream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { UnixStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_send(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Handle<File> {
    pub async fn open(p: impl AsRef<Path>) -> io::Result<Handle<File>> {
        let fd = Processor::processor_open_at(p).await?;
        dbg!(fd);
        let io = unsafe { File::from_raw_fd(fd as _) };

        Ok(Handle {
            io_task: Some(io),
            chan: None,
            store_file: Some(StoreFile::new(fd as _)),
            read: Arc::new(TTas::new(None)),
            write: Arc::new(TTas::new(None)),
        })
    }

    fn filler<T: AsRawFd>(cx: &mut Context, fd: T, pos: &mut usize, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        // let mut f = unsafe { File::from_raw_fd(fd.as_raw_fd()) };
        // let mut buffer = Vec::new();
        // f.read_to_end(&mut buffer).unwrap();
        // println!("BUFLEN ::::: {}", buffer.len());

        let fut = Processor::processor_read_file(&fd, buf, *pos);
        futures_util::pin_mut!(fut);

        let res = loop {
            match fut.as_mut().poll(cx)? {
                Poll::Ready(0) => {
                    dbg!("poll pend");
                    break Poll::Pending
                },
                Poll::Ready(n) => {
                    dbg!("poll read");
                    *pos += n;
                    break Poll::Ready(Ok(n))
                }
                _ => {
                    // break Poll::Pending
                }
            }
        };

        dbg!(&res);

        res
    }
}
