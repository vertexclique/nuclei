mod fs;
mod kqueue;
mod nethandle;
mod processor;

pub(crate) use fs::*;
pub(crate) use kqueue::*;

pub(crate) use processor::*;

pub const BACKEND: crate::sys::IoBackend = crate::sys::IoBackend::Kqueue;