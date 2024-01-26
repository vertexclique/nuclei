use crate::syscore::CompletionChan;
use crate::{Handle, Proactor};
use crossbeam_channel::RecvError;
use futures::Stream;
use pin_project_lite::pin_project;
use rustix::io_uring::{SocketFlags};
use rustix_uring::{opcode as OP, types::Fd};
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

pin_project! {
    ///
    /// TcpStream generator that is fed by multishot accept with multiple CQEs.
    #[derive(Clone)]
    pub struct TcpStreamGenerator {
        listener: RawFd,
        rx: CompletionChan
    }
}

impl TcpStreamGenerator {
    pub fn new<T: AsRawFd>(listener: &T) -> io::Result<Self> {
        let sqe = OP::AcceptMulti::new(Fd(listener.as_raw_fd()))
            .flags(SocketFlags::NONBLOCK)
            .build();

        let rx = Proactor::get().inner().register_io(sqe)?;

        Ok(Self {
            listener: listener.as_raw_fd(),
            rx,
        })
    }
}

impl Stream for TcpStreamGenerator {
    type Item = Handle<TcpStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this.rx.get_rx().recv() {
            Ok(sfd) => {
                let stream = unsafe { TcpStream::from_raw_fd(sfd) };
                let hs = Handle::new(stream).unwrap();
                Poll::Ready(Some(hs))
            }
            Err(e) if e == RecvError => Poll::Ready(None),
            _ => Poll::Pending,
        }
    }
}
