//! The descriptor a source works on, as each platform names it: a file
//! descriptor on unix; on Windows a `SOCKET`, or a file's `HANDLE` held as
//! the same integer.

#[cfg(unix)]
mod imp {
    use core::sync::atomic::AtomicI32;

    pub(crate) use std::os::fd::OwnedFd as Owned;
    pub(crate) use std::os::fd::OwnedFd as OwnedFile;
    pub(crate) use std::os::fd::RawFd as Raw;

    pub(crate) type AtomicRaw = AtomicI32;

    /// No descriptor.
    pub(crate) const NONE: Raw = -1;
}

#[cfg(windows)]
mod imp {
    use core::sync::atomic::AtomicU64;

    pub(crate) use std::os::windows::io::OwnedHandle as OwnedFile;
    pub(crate) use std::os::windows::io::OwnedSocket as Owned;

    /// A `SOCKET`, or a file `HANDLE` cast to the same width.
    pub(crate) type Raw = u64;

    pub(crate) type AtomicRaw = AtomicU64;

    /// No descriptor: `INVALID_SOCKET` and `INVALID_HANDLE_VALUE` alike.
    pub(crate) const NONE: Raw = u64::MAX;
}

pub(crate) use imp::*;
