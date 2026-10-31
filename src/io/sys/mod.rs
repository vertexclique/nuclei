//! The system calls of the driver and of the handles, per platform: the
//! unix ones made on file descriptors, and the Windows ones on sockets and
//! file handles.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use self::unix::*;
#[cfg(unix)]
mod space;
#[cfg(unix)]
pub(crate) use self::space::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use self::windows::*;
