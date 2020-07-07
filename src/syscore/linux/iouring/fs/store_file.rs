use crate::fs::buffer::Buffer;
use std::pin::Pin;
use std::fs::File;
use crate::Handle;
use std::io;
use std::sync::Arc;
use lever::sync::prelude::TTas;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use super::buffer::Buffer;

pub struct StoreFile {
    fd: RawFd,
    buf: Buffer,
    pos: usize,
}

impl StoreFile {
    fn new(fd: RawFd) -> StoreFile {
        StoreFile {
            fd,
            buf: Buffer::new(),
            pos: 0,
        }
    }

    #[inline(always)]
    fn bufstate(self: Pin<&mut Self>) -> (&mut Buffer, &mut usize) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (&mut this.buf, &mut this.pos)
        }
    }
}

// impl Handle<File> {
//     pub fn new(io: T) -> io::Result<Handle<T>>
//     where
//         T: AsRawFd
//     {
//         let fd = io.as_raw_fd();
//
//         Ok(Handle {
//             io_task: Some(io),
//             chan: None,
//             store_file: Some(StoreFile::new(fd)),
//             read: Arc::new(TTas::new(None)),
//             write: Arc::new(TTas::new(None)),
//         })
//     }
// }