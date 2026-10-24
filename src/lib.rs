//! # Nuclei
//!
//! A proactive IO runtime: thread-per-core shards that run tasks and drive
//! their own IO, timers and buffers. Every operation is completed for the
//! task that asked for it; no task waits for readiness and then makes the
//! call itself.
//!
//! IO runs on epoll on Linux and on kqueue on macOS and the BSDs. A
//! [`Handle`] wraps std's own socket or file types and gives them async
//! operations (`accept`, `connect`, `send`, `recv`, ...) and the
//! `futures-io` traits. [`spawn`], [`block_on`] and the rest place work on
//! the process default [`Runtime`], which starts on first use with no
//! configuration; a [`Builder`] sets every setting for a runtime of one's
//! own.
//!
//! ```no_run
//! use std::net::TcpListener;
//! use nuclei::Handle;
//!
//! #[nuclei::main]
//! async fn main() -> std::io::Result<()> {
//!     let listener = Handle::<TcpListener>::bind("127.0.0.1:7000")?;
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

mod buf;
mod compat;
/// Nuclei's configuration options reside here.
pub mod config;
#[cfg(unix)]
mod fs;
#[cfg(unix)]
mod io;
#[cfg(unix)]
mod net;
mod proactor;
mod rt;
/// nuclei's own runtime: shards, tasks, `block_on`, `spawn` and the blocking pool.
pub mod runtime;
/// Runtime-independent synchronization primitives: futures and wakers only, usable from any executor.
pub mod sync;
#[cfg(target_os = "linux")]
mod syscore {
    // The io_uring ring and its probe. Parts of the ring layer (provided
    // buffer rings, multishot entries, flags) are for the driver's next
    // steps; their own tests run them now.
    #![allow(dead_code)]
    pub(crate) mod linux;
}
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
)))]
compile_error!("Target OS is not supported");

#[cfg(target_os = "windows")]
compile_error!("nuclei's Windows backend (I/O completion ports) is not in this version");

pub use compat::{
    GlobalExecutorConfig, init, init_with_config, spawn_more_threads, stop_current_thread,
    stop_thread,
};
#[cfg(unix)]
pub use io::Handle;
#[cfg(unix)]
pub use net::TcpStreamGenerator;
#[cfg(feature = "attributes")]
pub use nuclei_macros::{bench, main, test};
pub use proactor::{IoBackend, Proactor, drive};
pub use runtime::{
    Builder, FallibleTask, Runtime, RuntimeHandle, Task, block_on, spawn, spawn_blocking,
    spawn_local,
};
pub use time::{sleep, timeout};
