use std::{task::Poll, fs::File, pin::Pin};
use super::handle::Handle;
use futures::io::AsyncRead;
use super::completion_handler::CompletionHandler;

#[cfg(unix)]
use std::{mem::ManuallyDrop, os::unix::io::{AsRawFd, FromRawFd}};
use crate::syscore::Processor;

//
// Proxy operations for Future registration via AsyncRead, AsyncWrite and others.
impl AsyncRead for &Handle<File> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        CompletionHandler::<Self>::handle_read(self, cx, async move {
            let file = unsafe { File::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_read(&file, buf).await?;

            let _ = ManuallyDrop::new(file);
            Ok(size)
        })
    }
}
