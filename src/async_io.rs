use std::{io, task};
use std::{task::Poll, fs::File, pin::Pin, task::Context};
use super::handle::Handle;
use futures::io::{AsyncRead, AsyncWrite};
use super::submission_handler::SubmissionHandler;
use std::io::{Read, BufRead};

use std::net::{TcpStream};

#[cfg(unix)]
use std::{mem::ManuallyDrop, os::unix::io::{AsRawFd, FromRawFd}};
#[cfg(unix)]
use std::os::unix::net::{UnixStream};

use crate::syscore::Processor;
use futures::AsyncBufRead;

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


impl_async_read!(File);
impl_async_write!(File);
impl_async_read!(TcpStream);
impl_async_write!(TcpStream);

#[cfg(unix)]
impl_async_read!(UnixStream);
#[cfg(unix)]
impl_async_write!(UnixStream);


///////////////////////////////////
///// File
///////////////////////////////////

#[cfg(unix)]
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

// #[cfg(unix)]
// impl AsyncBufRead for &Handle<File> {
//     fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
//         Poll::Ready(io::BufRead::fill_buf(self.get_mut()))
//     }
//
//     fn consume(self: Pin<&mut Self>, amt: usize) {
//         io::BufRead::consume(self.get_mut(), amt)
//     }
// }

#[cfg(unix)]
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