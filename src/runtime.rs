//! nuclei's own runtime: thread-per-core shards that run tasks, a blocking
//! pool, and the functions that place work on them.
//!
//! A [`Runtime`] runs one shard per worker thread. A shard owns its run
//! queue, its timers and its poller, and a task stays on the shard it was
//! placed on: [`spawn`] keeps it near the spawning thread's other tasks
//! within a margin of load (see [`Builder::local_spawn_margin`]),
//! [`spawn_local`] keeps it on the calling thread. [`block_on`] runs a
//! future on the calling thread and nests: called from a task, it keeps the
//! shard running its other tasks until the future completes.
//!
//! The free functions use the runtime the calling thread is inside, or the
//! process default runtime, which starts on first use with no configuration
//! (one worker per core). [`Builder`] sets everything else.
//!
//! ```
//! use nuclei::runtime::{Runtime, spawn};
//!
//! let runtime = Runtime::builder().worker_threads(2).build().unwrap();
//! let sum = runtime.block_on(async {
//!     let tasks: Vec<_> = (0..4u64).map(|n| spawn(async move { n * n })).collect();
//!     let mut sum = 0;
//!     for task in tasks {
//!         sum += task.await;
//!     }
//!     sum
//! });
//! assert_eq!(sum, 14);
//! ```

pub use crate::rt::runtime::{
    Builder, Runtime, RuntimeHandle, block_on, spawn, spawn_blocking, spawn_local,
};
pub use crate::rt::task::{FallibleTask, Task};
