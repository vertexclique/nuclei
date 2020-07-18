mod handle;
mod proactor;
mod runtime;
mod submission_handler;
mod sys;
mod utils;
mod waker;

#[cfg(not(any(
    target_os = "linux",     // epoll, iouring
    target_os = "android",   // epoll
    target_os = "illumos",   // epoll
    target_os = "macos",     // kqueue
    target_os = "ios",       // kqueue
    target_os = "freebsd",   // kqueue
    target_os = "netbsd",    // kqueue
    target_os = "openbsd",   // kqueue
    target_os = "dragonfly", // kqueue
    target_os = "windows",   // iocp
)))]
compile_error!("Target OS is not supported");

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
mod syscore {
    mod bsd;
    pub(crate) use bsd::*;
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
mod syscore {
    mod linux;
    pub(crate) use linux::*;
}

#[cfg(target_os = "windows")]
mod syscore {
    mod windows;
    pub(crate) use windows::*;
}

pub use agnostik::*;
pub use proactor::*;
