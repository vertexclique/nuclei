use super::Processor;
use crate::handle::Handle;
use crate::submission_handler::SubmissionHandler;
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::net::TcpStream;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::ptr;
use std::time::Duration;
use std::{io, task};
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
    task::Poll,
};
use winapi::um::handleapi;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::ioapiset;
use winapi::um::winbase::INFINITE;
use winapi::um::winnt::HANDLE;

pub struct SysProactor {
    iocp_handle: HANDLE,
    completions: HashMap<HANDLE, oneshot::Sender<usize>>,
    // ULONG_PTR
    completion_key: AtomicUsize,
}

// TODO [igni]: SUPER CURSED THIS NEEDS TO GO AWAY
unsafe impl Send for SysProactor {}
unsafe impl Sync for SysProactor {}

impl SysProactor {
    /// Returns a reference to the proactor.
    pub fn new() -> Result<Self, ()> {
        let completion_key = AtomicUsize::default();
        let iocp_handle = try_create_iocp(
            INVALID_HANDLE_VALUE,
            ptr::null_mut(),
            completion_key.fetch_add(1, Ordering::SeqCst),
        )
        .unwrap();

        Ok(Self {
            iocp_handle,
            completion_key,
            completions: Default::default(),
        })
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
        &self
    }

    pub(crate) fn register_io(&self, file: RawHandle) -> io::Result<oneshot::Receiver<usize>> {
        let (sender, receiver) = oneshot::channel();
        // Send stuff
        let mut completion_key = self.completion_key();
        let handle = try_create_iocp(file as _, self.iocp_handle, completion_key)?;

        // TODO [igni]: threads

        // Wait for completion
        let mut bytes_transferred = 0;
        let mut actual_completion_key = 0;
        let mut overlapped = ptr::null_mut();
        let done = unsafe {
            ioapiset::GetQueuedCompletionStatus(
                handle,
                &mut bytes_transferred,
                &mut completion_key,
                overlapped,
                INFINITE, // TODO: Play around with that on poll or something.
            )
        };

        if done == 0 {
            println!("something terrible happened");
        }

        // cast is safe here because bytes_transferred is a u32
        sender.send(bytes_transferred as usize);

        Ok(receiver)
    }

    fn completion_key(&self) -> usize {
        self.completion_key.fetch_add(1, Ordering::SeqCst)
    }
}

fn try_create_iocp(
    file_handle: HANDLE,
    existing_completion_port: HANDLE,
    completion_key: usize,
) -> io::Result<HANDLE> {
    let task_handle = unsafe {
        ioapiset::CreateIoCompletionPort(file_handle, existing_completion_port, completion_key, 0)
    };

    if task_handle.is_null() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Couldn't create iocp - {}", io::Error::last_os_error()),
        ))
    } else {
        Ok(task_handle)
    }
}

impl Drop for SysProactor {
    fn drop(&mut self) {
        // If the function succeeds, the return value is nonzero.
        if unsafe { handleapi::CloseHandle(self.iocp_handle) } == 0 {
            println!(
                "warning : couldn't drop iocp handle - {}",
                io::Error::last_os_error()
            );
        };
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
        let raw_handle = self.as_raw_handle();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_handle(raw_handle) };
            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };

            let size = Processor::processor_read_file(&file, buf).await?;
            let _ = ManuallyDrop::new(file);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(windows)]
impl AsyncWrite for &Handle<File> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let raw_handle = self.as_raw_handle();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_handle(raw_handle) };
            let mut buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };

            let size = Processor::processor_write_file(&file, &mut buf).await?;
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
