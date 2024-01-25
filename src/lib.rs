//!
//! # Nuclei
//!
//! Nuclei is a proactive IO system which is runtime agnostic and can work with any runtime.
//! Proactive system's design principles matching to [Boost Asio](https://www.boost.org/doc/libs/1_47_0/doc/html/boost_asio/overview/core/async.html).
//! Nuclei is not using conventional reactor approach. It is completely asynchronous, and it's wrapping poll based IO proactive fashion.
//!
//! Nuclei uses [epoll](https://en.wikipedia.org/wiki/Epoll) on Linux as primary evented IO backend, secondarily (if your system supports) you can use
//! [io_uring](https://kernel.dk/io_uring.pdf). On MacOS, Nuclei is using [kqueue](https://en.wikipedia.org/wiki/Kqueue).
//! On Windows, [IOCP](https://en.wikipedia.org/wiki/Input/output_completion_port) backend is used.
//!
//! Current io_uring implementation needs Linux kernel 5.6+.
//!
//! ## Features
//!
//! * Async TCP, UDP, Unix domain sockets and files...
//! * Proactor system don't block.
//! * Scatter/Gather operations
//! * Minimal allocation as much as possible.
//! * More expressive than any other runtime.
//! * Completely asynchronous I/O system with lock free programming.
//!
//! ## Examples
//! For more information about how to use Nuclei with std IO types please head to [examples](https://github.com/vertexclique/nuclei/tree/master/examples).
//!
//! ### Executor
//! Executor is using `async-global-executor`. Available features are:
//! * `async-exec`: Uses `async-io` feature of `async-global-executor`.
//! * `tokio`: Uses tokio

// These need to go through time.
#![allow(dead_code, unused_variables)]

mod async_io;
mod handle;
mod proactor;
mod submission_handler;
mod sys;
mod utils;
mod waker;
/// Nuclei's configuration options reside here.
pub mod config;

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

pub use async_global_executor::*;
pub use proactor::*;

#[cfg(feature = "attributes")]
pub use nuclei_attributes::*;