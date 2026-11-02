//! Tasks placed on a worker the caller names: [`RuntimeHandle::workers`],
//! [`spawn_on_worker`](RuntimeHandle::spawn_on_worker),
//! [`spawn_local_on_worker`](RuntimeHandle::spawn_local_on_worker) and
//! [`on_each_worker`](RuntimeHandle::on_each_worker).
//!
//! A task placed on worker `k` is a task of `k`'s shard, as one spawned
//! there is, and stays on it. The handoff moves a shard with every task of
//! it to another thread (on the same core, when workers are pinned): the
//! task keeps running on worker `k`, and
//! [`worker_index`](super::worker_index) keeps answering `k`. A worker that does not take tasks refuses the placement with
//! `InvalidInput`, and so does one that begins to stop while the task is
//! placed: the task is counted on the shard before the shard is asked again
//! whether it takes tasks, so a stop either sees the task (and runs it to
//! its end) or the spawn sees the stop (and drops the task unrun). No placed
//! task is ever accepted and then dropped unrun (proofs/tla/placement).

use core::future::Future;
use std::io;
use std::sync::Arc;

use super::super::sched::Sched;
use super::super::shard::Shared;
use super::super::task::{self, Task};
use super::{Inner, RuntimeHandle, spawned};
use crate::sync::internal::{Ordering, fence};
use crate::task::JoinSet;

impl RuntimeHandle {
    /// The workers the runtime has started, stopped ones included: the
    /// indices [`spawn_on_worker`](Self::spawn_on_worker) takes are
    /// `0..workers()`.
    pub fn workers(&self) -> usize {
        self.inner.workers.started()
    }

    /// Spawns `future` on worker `worker`, where it stays: a handoff of the
    /// worker's shard moves the task with it, so it runs on that worker for
    /// its whole life.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when the runtime has no such worker, or the worker
    /// stopped (or is stopping) and takes no new task.
    pub fn spawn_on_worker<F>(&self, worker: usize, future: F) -> io::Result<Task<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        spawn_on_worker(&self.inner, worker, future)
    }

    /// Runs `make` on worker `worker` and spawns the future it makes there,
    /// which need not be `Send`: it stays on the thread that made it, as a
    /// [`spawn_local`](crate::spawn_local) task does.
    ///
    /// A handoff of the worker's shard leaves such a future on the thread
    /// that made it, off the worker: it finishes there, where
    /// [`worker_index`](super::worker_index) answers `None` and shard-local
    /// values are out of reach. The handle is that of the task that makes
    /// the future on the worker and waits for it: its id is not the one
    /// [`task::id`](crate::task::id) answers inside the future, and dropping
    /// it cancels both.
    ///
    /// # Errors
    ///
    /// As for [`spawn_on_worker`](Self::spawn_on_worker).
    pub fn spawn_local_on_worker<M, F>(&self, worker: usize, make: M) -> io::Result<Task<F::Output>>
    where
        M: FnOnce() -> F + Send + 'static,
        F: Future + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_on_worker(worker, async move {
            let task = crate::spawn_local(make());
            task.await
        })
    }

    /// Runs `make` on every worker that takes tasks and spawns the future it
    /// makes there, as [`spawn_local_on_worker`](Self::spawn_local_on_worker)
    /// does: the set holds one task per worker. A runtime with no worker
    /// gives an empty set.
    pub fn on_each_worker<M, F>(&self, make: M) -> JoinSet<F::Output>
    where
        M: Fn() -> F + Send + Sync + 'static,
        F: Future + 'static,
        F::Output: Send + 'static,
    {
        let make = Arc::new(make);
        let mut set = JoinSet::new();
        for worker in 0..self.workers() {
            let make = Arc::clone(&make);
            // A worker that stopped is no worker to run on.
            if let Ok(task) = self.spawn_local_on_worker(worker, move || make()) {
                set.insert(task);
            }
        }
        set
    }
}

/// The shard of worker `worker` of `inner`, if it takes tasks.
fn worker_shard(inner: &Inner, worker: usize) -> io::Result<&Arc<Shared>> {
    let started = inner.workers.started();
    if worker >= started {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("worker {worker} does not exist: the runtime has started {started} workers"),
        ));
    }
    match inner.workers.get(worker) {
        Some(shard) if shard.takes_tasks() => Ok(shard),
        Some(_) => Err(stopped(worker)),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("worker {worker} never started: its shard could not be made"),
        )),
    }
}

fn stopped(worker: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("worker {worker} has stopped and takes no new task"),
    )
}

/// Spawns `future` on worker `worker` of `inner`: as `spawn_on` does, with
/// the second look at the worker between the task's count and its schedule.
pub(super) fn spawn_on_worker<F>(
    inner: &Inner,
    worker: usize,
    future: F,
) -> io::Result<Task<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let shard = worker_shard(inner, worker)?;
    let hooks = inner.task_hooks.as_ref();
    let (runnable, task) = task::spawn(
        future,
        Sched::<false>::counted(Arc::clone(shard), None, hooks.cloned()),
    );
    spawned(hooks.map(|hooks| &**hooks), &task);
    // Pairs with the fence of `Shared::drain`: either the stop sees the
    // task counted, or this sees the stop.
    fence(Ordering::SeqCst);
    if !shard.takes_tasks() {
        // Never run: its future is dropped here, and its handle with it.
        drop(runnable);
        drop(task);
        return Err(stopped(worker));
    }
    runnable.schedule();
    Ok(task)
}
