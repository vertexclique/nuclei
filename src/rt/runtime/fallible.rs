//! The spawns of [`task::Builder`](crate::task::Builder), which answer an
//! error wherever the free functions panic: a default runtime or a thread's
//! shard that cannot start, a worker that takes no task, a future that is
//! not `Send` named for a worker it cannot reach.

use core::future::Future;
use std::io;
use std::sync::Arc;

use super::super::context;
use super::{DEFAULT, Inner, Runtime, Task, context_hooks, spawn_here, spawn_on, spawn_unworked};

/// The process default runtime, started now if it is not running; the
/// error of its start, if it cannot.
pub(super) fn try_default() -> io::Result<&'static Runtime> {
    if let Some(runtime) = DEFAULT.get() {
        return Ok(runtime);
    }
    // One start at a time: a start that fails leaves the next caller to try
    // again, and two that race never both start workers.
    static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(runtime) = DEFAULT.get() {
        return Ok(runtime);
    }
    let runtime = super::Builder::new().build()?;
    // `build_global` may have set one meanwhile: that one stays.
    Ok(DEFAULT.get_or_init(|| runtime))
}

/// The runtime this thread is inside, or the default one.
pub(crate) fn try_current() -> io::Result<Arc<Inner>> {
    match context::runtime() {
        Some(inner) => Ok(inner),
        None => try_default().map(|runtime| Arc::clone(&runtime.inner)),
    }
}

/// Makes this thread's own shard now unless it runs one, so a spawn on it
/// cannot fail.
fn ensure_shard() -> io::Result<()> {
    if context::current(|current| current.is_some()) {
        return Ok(());
    }
    context::own_shard().map(drop)
}

/// [`spawn`](super::spawn) on `inner`, or on worker `worker` of it.
pub(crate) fn try_spawn<F>(
    inner: &Arc<Inner>,
    worker: Option<usize>,
    future: F,
) -> io::Result<Task<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if let Some(worker) = worker {
        return super::placement::spawn_on_worker(inner, worker, future);
    }
    match inner.workers.place(inner.id, inner.local_margin) {
        Some(shard) => Ok(spawn_on(inner, shard, future)),
        None => {
            ensure_shard()?;
            Ok(spawn_unworked(inner, future))
        }
    }
}

/// [`spawn_local`](super::spawn_local), on worker `worker` if named, which
/// must be the one this thread runs: a future that is not `Send` cannot go
/// to another.
pub(crate) fn try_spawn_local<F>(worker: Option<usize>, future: F) -> io::Result<Task<F::Output>>
where
    F: Future + 'static,
    F::Output: 'static,
{
    if let Some(worker) = worker
        && super::worker_index() != Some(worker)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a future that is not Send spawns only on the worker that runs it, not on worker \
                 {worker}: use RuntimeHandle::spawn_local_on_worker to make it there"
            ),
        ));
    }
    ensure_shard()?;
    Ok(spawn_here::<_, true>(future, context_hooks()))
}

/// [`spawn_blocking`](super::spawn_blocking); a worker named for it is
/// refused, as the blocking pool runs no worker.
pub(crate) fn try_spawn_blocking<F, T>(worker: Option<usize>, f: F) -> io::Result<Task<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if let Some(worker) = worker {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the blocking pool runs no worker, so a blocking call cannot go to worker \
                 {worker}: spawn it without one"
            ),
        ));
    }
    Ok(try_current()?.pool.spawn(f))
}
