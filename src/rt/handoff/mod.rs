//! The shard handoff: a worker blocked in one poll (a blocking call, a long
//! loop) hands its shard to another thread, so the shard's other tasks,
//! timers and IO run on.
//!
//! The shard stays the same object with the same `Shared`, so every task,
//! waker, registration and handle keeps pointing at the right place; only
//! the thread under it changes. The runtime's [`Monitor`] sees a worker past
//! the threshold in one poll, takes its shard through the worker's
//! [`Slot`](slot::Slot), and hands it to a spare thread. The blocked thread
//! finishes its poll later, finds the shard gone, and carries on as a thread
//! with no shard: a `block_on` frame on its stack moves its future to the
//! thread's own shard, and its bound tasks (`spawn_local`, whose futures may
//! not move) wait in its [`Home`](home::Home) and run there too; then it
//! waits as a spare.
//!
//! What the worker pays: two plain stores and a load per poll, and a store, a
//! load and a store per call into the runtime from a task's code (the
//! compiler's fence between, where the process has a heavy fence, see
//! [`fence`]; a full fence elsewhere). proofs/tla/handoff checks the
//! protocol, with the store buffers of x86 and every mutation of it that
//! must fail.

pub(crate) mod fence;
pub(crate) mod home;
pub(crate) mod life;
mod monitor;
pub(crate) mod slot;
pub(crate) mod target;

pub(crate) use monitor::{Gift, Monitor, Owned, Runner, Spare, run};

/// Whether a runtime may watch its workers here: not under loom (whose
/// threads are its own), not where there are no threads.
pub(crate) const SUPPORTED: bool = cfg!(not(any(all(loom, test), target_family = "wasm")));
