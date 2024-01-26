pub(crate) mod fs;
mod iouring;
mod net;
mod nethandle;
mod processor;

pub(crate) use fs::*;
pub(crate) use iouring::*;

pub(crate) use processor::*;

pub const BACKEND: crate::sys::IoBackend = crate::sys::IoBackend::IoUring;
