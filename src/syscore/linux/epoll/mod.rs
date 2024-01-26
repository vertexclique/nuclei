mod epoll;
mod fs;
mod nethandle;
mod processor;

pub(crate) use epoll::*;
pub(crate) use fs::*;

pub(crate) use processor::*;

pub const BACKEND: crate::sys::IoBackend = crate::sys::IoBackend::Epoll;
