use crate::Handle;
use std::io;
use lever::sync::prelude::*;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

impl<T: AsRawFd> Handle<T> {
    pub fn new(io: T) -> io::Result<Handle<T>> {
        Ok(Handle {
            io_task: Some(io),
            read: Arc::new(TTas::new(None)),
            write: Arc::new(TTas::new(None)),
        })
    }
}