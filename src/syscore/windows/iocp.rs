use crate::handle::Handle;
use crate::submission_handler::SubmissionHandler;
use futures::io::{AsyncRead, AsyncWrite};
use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::net::TcpStream;
use std::time::Duration;
use std::{io, task};
use std::{pin::Pin, task::Context, task::Poll};

pub struct SysProactor {}

impl SysProactor {
    /// Returns a reference to the proactor.
    pub fn new() -> Result<Self, ()> {
        Ok(Self {})
    }

    /// Wakes the thread waiting on proactor.
    pub fn wake(&self) -> io::Result<()> {
        Ok(())
    }

    /// Wait for completion of IO object
    pub fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        todo!();
    }

    /// Get underlying proactor instance.
    pub(crate) fn inner(&self) -> &SysProactor {
        todo!();
    }
}

///////////////////////////////////
///// File
///////////////////////////////////

#[cfg(windows)]
impl AsyncRead for &Handle<File> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        todo!();
        // let raw_fd = self.as_raw_fd();
        // let buf_len = buf.len();
        // let buf = buf.as_mut_ptr();

        // let completion_dispatcher = async move {
        //     let file = unsafe { File::from_raw_fd(raw_fd) };

        //     let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
        //     let size = Processor::processor_read_file(&file, buf).await?;

        //     let _ = ManuallyDrop::new(file);
        //     Ok(size)
        // };

        // SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(windows)]
impl AsyncWrite for &Handle<File> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        todo!();
        // let raw_fd = self.as_raw_fd();
        // let buf_len = buf.len();
        // let buf = buf.as_ptr();

        // let completion_dispatcher = async move {
        //     let file = unsafe { File::from_raw_fd(raw_fd) };

        //     let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
        //     let size = Processor::processor_write_file(&file, buf).await?;

        //     let _ = ManuallyDrop::new(file);
        //     Ok(size)
        // };

        // SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
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

#[cfg(windows)]
impl AsyncRead for &Handle<TcpStream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        todo!();
        // let raw_fd = self.as_raw_fd();
        // let buf_len = buf.len();
        // let buf = buf.as_mut_ptr();

        // let completion_dispatcher = async move {
        //     let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

        //     let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
        //     let size = Processor::processor_recv(&sock, buf).await?;

        //     let _ = ManuallyDrop::new(sock);
        //     Ok(size)
        // };

        // SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(windows)]
impl AsyncWrite for &Handle<TcpStream> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        todo!();
        // let raw_fd = self.as_raw_fd();
        // let buf_len = buf.len();
        // let buf = buf.as_ptr();

        // let completion_dispatcher = async move {
        //     let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

        //     let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
        //     let size = Processor::processor_send(&sock, buf).await?;

        //     let _ = ManuallyDrop::new(sock);
        //     Ok(size)
        // };

        // SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
