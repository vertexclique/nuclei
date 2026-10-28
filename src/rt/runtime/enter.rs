//! A runtime set on a thread without `block_on` ([`Runtime::enter`]), the
//! handle of the runtime a thread is inside without falling back to the
//! default one ([`RuntimeHandle::try_current`]), runtime ids, the worker a
//! thread is, and a shutdown that does not wait.

use core::fmt;
use core::marker::PhantomData;
use std::sync::Arc;

use super::{Inner, Runtime, RuntimeHandle};
use crate::rt::context;

/// A runtime's id, distinct among the runtimes of the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeId(u64);

impl fmt::Display for RuntimeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The runtime [`Runtime::enter`] set on this thread, until it drops: the
/// free functions ([`spawn`](crate::spawn), ...) use that runtime meanwhile.
/// It stays on the thread that made it.
#[must_use = "the runtime is set only while the guard lives"]
pub struct EnterGuard<'a> {
    _set: context::RuntimeSet,
    _runtime: PhantomData<&'a ()>,
    _not_send: PhantomData<*const ()>,
}

impl fmt::Debug for EnterGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnterGuard").finish_non_exhaustive()
    }
}

/// [`RuntimeHandle::try_current`] found no runtime set on this thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TryCurrentError {
    _private: (),
}

impl TryCurrentError {
    /// Whether no runtime was set on the thread (the one reason today).
    pub fn is_missing_context(&self) -> bool {
        true
    }
}

impl fmt::Display for TryCurrentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "no runtime is set on this thread: call it inside `block_on`, a task, or `enter`",
        )
    }
}

impl std::error::Error for TryCurrentError {}

fn enter(inner: &Arc<Inner>) -> EnterGuard<'_> {
    EnterGuard {
        _set: context::set_runtime(inner),
        _runtime: PhantomData,
        _not_send: PhantomData,
    }
}

impl Runtime {
    /// Sets this runtime on the calling thread until the guard drops, so the
    /// free functions use it outside `block_on`.
    ///
    /// ```
    /// let runtime = nuclei::Runtime::builder().worker_threads(1).build().unwrap();
    /// let _guard = runtime.enter();
    /// let task = nuclei::spawn(async { 2 + 2 });
    /// assert_eq!(runtime.block_on(task), 4);
    /// ```
    pub fn enter(&self) -> EnterGuard<'_> {
        enter(&self.inner)
    }

    /// The runtime's id.
    pub fn id(&self) -> RuntimeId {
        RuntimeId(self.inner.id)
    }

    /// Stops the workers and returns at once: the worker threads drop their
    /// tasks and exit on their own, without being waited for.
    pub fn shutdown_background(self) {
        self.inner.workers.stop_all();
        // Detached: dropping the handles does not wait. `Drop` finds none
        // left to join.
        drop(self.inner.workers.take_threads());
    }
}

impl RuntimeHandle {
    /// Sets the handle's runtime on the calling thread until the guard
    /// drops.
    pub fn enter(&self) -> EnterGuard<'_> {
        enter(&self.inner)
    }

    /// The runtime's id.
    pub fn id(&self) -> RuntimeId {
        RuntimeId(self.inner.id)
    }

    /// The handle of the runtime set on this thread (inside its `block_on`,
    /// on one of its workers, or under [`enter`](Self::enter)); unlike
    /// [`current`](Self::current) it never falls back to the default
    /// runtime.
    pub fn try_current() -> Result<Self, TryCurrentError> {
        context::runtime()
            .map(|inner| Self { inner })
            .ok_or(TryCurrentError { _private: () })
    }
}

/// The index of the worker this code runs on (0, 1, ... among the workers
/// of its runtime), `None` off the workers (in `block_on`'s own thread, on
/// the blocking pool, outside any runtime).
pub fn worker_index() -> Option<usize> {
    let runtime = context::runtime()?;
    context::current(|shard| shard.and_then(|shard| shard.worker_index(runtime.id)))
}
