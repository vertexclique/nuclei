use crate::Handle;
use lever::sync::prelude::TTas;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;

use super::buffer::Buffer;
use crate::syscore::Processor;
use lever::sync::atomics::AtomicBox;
use pin_utils::unsafe_pinned;
use std::task::{Context, Poll};

pub struct StoreFile {
    fd: RawFd,
    buf: Buffer,
    pub internal: Vec<u8>,
    op_state: Arc<AtomicBox<Op>>,
    pos: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Op {
    Read,
    ReadMore,
    Write,
    Close,
    Nothing,
    Pending,
    Statx,
}

impl StoreFile {
    pub(crate) fn new(fd: RawFd) -> StoreFile {
        StoreFile {
            fd,
            op_state: Arc::new(AtomicBox::new(Op::Nothing)),
            buf: Buffer::new(),
            internal: Vec::with_capacity(8192),
            pos: 0,
        }
    }

    #[inline(always)]
    pub(crate) fn op_state(&self) -> Arc<AtomicBox<Op>> {
        self.op_state.clone()
    }

    #[inline(always)]
    pub(crate) fn bufpair(&mut self) -> (&mut Buffer, &mut usize) {
        (&mut self.buf, &mut self.pos)
    }

    pub(crate) fn from_fd_to_file(&self) -> File {
        unsafe { File::from_raw_fd(self.fd) }
    }

    pub(crate) fn receive_fd(&self) -> RawFd {
        self.fd
    }

    #[inline(always)]
    pub(crate) fn bufstate(self: Pin<&mut Self>) -> (&mut Buffer, &mut usize) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (&mut this.buf, &mut this.pos)
        }
    }

    pub(crate) async fn poll_file_size(&mut self) -> io::Result<usize> {
        self.op_state().replace_with(|_| Op::Statx);
        let fd = self.receive_fd();
        let (buf, _) = self.bufpair();
        let statx = buf.as_statx();

        Processor::processor_file_size(&fd, statx).await
    }

    #[inline(always)]
    pub(crate) fn buf(&mut self) -> &mut Buffer {
        &mut self.buf
    }

    #[inline(always)]
    pub(crate) fn pos(&mut self) -> &mut usize {
        &mut self.pos
    }

    pub(crate) fn guard_op(self: &mut Self, op: Op) {
        // let this = unsafe { Pin::get_unchecked_mut(self) };
        // if *self.op_state.get() != Op::Pending && *self.op_state.get() != op {
        //     self.cancel();
        // }

        // if *self.op_state.get() == Op::Pending {
        //     self.cancel();
        // }

        self.op_state.replace_with(|_| op);
    }

    pub(crate) fn cancel(&mut self) {
        self.op_state.replace_with(|_| Op::Nothing);
        self.buf.cancellation();
    }
}
