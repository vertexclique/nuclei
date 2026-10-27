//! # Nuclei
//!
//! Proactive IO runtime: thread-per-core shards that run tasks and drive
//! their own IO, timers and buffers. Every operation is completed for the
//! task that asked for it; no task waits for readiness and then makes the
//! call itself.
//!
//! IO runs on io_uring on Linux where the kernel allows it, else on epoll;
//! on kqueue on macOS and the BSDs; and on I/O completion ports on Windows.
//! A [`Handle`] wraps std's own socket or file types and gives them async
//! operations (`accept`, `connect`, `send`, `recv`, ...) and the
//! `futures-io` traits. [`spawn`], [`block_on`] and the rest place work on
//! the process default [`Runtime`], which starts on first use with no
//! configuration; a [`Builder`] sets every setting for a runtime of one's
//! own. [`stats()`] takes a snapshot of what every shard is doing: its
//! tasks, polls, IO in flight, buffers, timers and errors.
//!
//! A call that needs the default runtime or the calling thread's shard
//! panics if that cannot start (the system refused its poller: no
//! descriptors or memory left); [`Builder::build`] and [`task::Builder`]
//! answer the error instead.
//!
//! ```no_run
//! use nuclei::net::TcpListener;
//!
//! #[nuclei::main]
//! async fn main() -> std::io::Result<()> {
//!     let listener = TcpListener::bind("127.0.0.1:7000").await?;
//!     loop {
//!         let (stream, _) = listener.accept().await?;
//!         nuclei::spawn(async move {
//!             let mut buf = [0u8; 1024];
//!             while let Ok(n @ 1..) = stream.recv(&mut buf).await {
//!                 if stream.send(&buf[..n]).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         })
//!         .detach();
//!     }
//! }
//! ```

// A model-checking build (`--cfg loom`) compiles the loom models and leaves
// the ordinary tests out, so the helpers only those tests use go unused.
#![cfg_attr(loom, allow(dead_code, unused_imports))]
// No unwrap, expect or panic outside a documented contract, each marked
// where it is; every unsafe block names what it rests on. Tests and the
// model-checking build may assert.
#![cfg_attr(
    not(any(test, loom)),
    warn(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::undocumented_unsafe_blocks
    )
)]

mod buf;
mod compat;
/// Nuclei's configuration options reside here.
pub mod config;
pub mod fs;
pub mod io;
pub mod net;
mod proactor;
/// Child processes: spawn one, wait for its exit and talk to it through async pipes.
pub mod process;
mod rt;
/// nuclei's own runtime: shards, tasks, `block_on`, `spawn` and the blocking pool.
pub mod runtime;
/// Signals (unix) and console control events (Windows), as streams.
pub mod signal;
pub mod stats;
/// Runtime-independent synchronization primitives: futures and wakers only, usable from any executor.
pub mod sync;
#[cfg(any(target_os = "linux", target_os = "android", windows))]
mod syscore {
    // The io_uring ring and its probe. Parts of the ring layer (provided
    // buffer rings, multishot entries, flags) are for the driver's next
    // steps; their own tests run them now.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[allow(dead_code)]
    pub(crate) mod linux;
    // The kernel32, ws2_32 and ntdll interface of the IOCP driver.
    #[cfg(windows)]
    pub(crate) mod windows;
}
#[cfg(test)]
mod alone;
mod coop;
mod join_set;
#[doc(hidden)]
pub mod macros;
/// Tasks: spawning, their handles and ids, cancellation from anywhere, and yielding.
pub mod task;
mod task_local;
/// Time: sleeping, timeouts and intervals on the shard's timer wheel.
pub mod time;

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
    windows,                 // I/O completion ports
)))]
compile_error!("Target OS is not supported");

pub use compat::{
    GlobalExecutorConfig, init, init_with_config, spawn_more_threads, stop_current_thread,
    stop_thread,
};
pub use io::{Handle, IoObject};
pub use net::TcpStreamGenerator;
#[cfg(feature = "attributes")]
pub use nuclei_macros::{bench, main, test};
pub use proactor::{IoBackend, Proactor, drive};
pub use runtime::{
    Builder, FallibleTask, Runtime, RuntimeHandle, Task, block_on, spawn, spawn_blocking,
    spawn_local,
};
pub use stats::stats;
pub use time::{sleep, timeout};
