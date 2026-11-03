//! Tasks: spawning (named, or on a worker the caller names, or neither),
//! their handles and ids, cancellation from anywhere, and yielding to the
//! other tasks of the shard.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

pub use crate::join_set::JoinSet;

/// Cooperative scheduling: the budget of operations each poll of a task may
/// complete before it yields to the other tasks of its shard. IO that
/// completes spends one, and so do the receives, acquires and locks of
/// [`sync`](crate::sync) that complete in the poll that began them (see its
/// module docs).
pub mod coop {
    pub use crate::coop::{
        BUDGET, Cooperative, Unconstrained, consume_budget, cooperative, has_budget_remaining,
        unconstrained,
    };
}
pub use crate::rt::task::{AbortHandle, FallibleTask, Id, Join, JoinError, Task};
pub use crate::runtime::{spawn, spawn_blocking, spawn_local};
pub use crate::task_local::{AccessError, LocalKey, TaskLocalFuture};

/// The id of the task this code runs in.
///
/// # Panics
///
/// Outside a task: in `block_on`'s own future, or on a blocking-pool thread.
/// [`try_id`] answers `None` there instead.
#[expect(
    clippy::expect_used,
    reason = "documented under `# Panics`; `try_id` answers `None` instead"
)]
pub fn id() -> Id {
    try_id().expect("`nuclei::task::id` called outside a task")
}

/// The id of the task this code runs in, `None` outside a task (in
/// `block_on`'s own future, or on a blocking-pool thread).
pub fn try_id() -> Option<Id> {
    crate::rt::task::current_id()
}

/// Gives the other tasks of this shard a turn: the task goes to the back of
/// its shard's run queue and runs again after the tasks queued before it. In
/// `block_on`'s own future it lets the shard run its queue once.
pub async fn yield_now() {
    YieldNow { yielded: false }.await
}

/// Pending once, waking itself: a task woken during its own poll is queued
/// behind the others.
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Spawns tasks with options: a name, kept with the task's id while the
/// task lives and shown to the runtime's task hooks
/// ([`TaskMeta::name`](crate::runtime::TaskMeta::name)), and the worker to
/// place it on. Every spawn answers an error where the free functions
/// panic.
///
/// ```
/// # nuclei::block_on(async {
/// let task = nuclei::task::Builder::new().name("fetch").spawn(async { 7 })?;
/// assert_eq!(task.await, 7);
/// # std::io::Result::Ok(())
/// # }).unwrap();
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct Builder<'a> {
    name: Option<&'a str>,
    worker: Option<usize>,
}

std::thread_local! {
    /// The name a builder's spawn on this thread gives its task, until the
    /// spawn names it (before the hooks see the task) or the builder does.
    static NAMING: core::cell::RefCell<Option<std::sync::Arc<str>>> =
        const { core::cell::RefCell::new(None) };
}

/// Names the task `id`, just made by a builder's spawn on this thread, with
/// the builder's name, before any hook of its runtime sees it.
pub(crate) fn name_now(id: Id) {
    if let Some(name) = NAMING
        .try_with(|naming| naming.borrow_mut().take())
        .ok()
        .flatten()
    {
        crate::rt::task::names::set(id, &name);
    }
}

impl<'a> Builder<'a> {
    /// A builder with no option set.
    pub fn new() -> Self {
        Self {
            name: None,
            worker: None,
        }
    }

    /// Names the task.
    pub fn name(self, name: &'a str) -> Self {
        Self {
            name: Some(name),
            ..self
        }
    }

    /// Places the task on worker `worker` of its runtime, where it stays, as
    /// [`RuntimeHandle::spawn_on_worker`](crate::runtime::RuntimeHandle::spawn_on_worker)
    /// does. A [`spawn_local`](Self::spawn_local) then needs to run on that
    /// worker, and a [`spawn_blocking`](Self::spawn_blocking) is refused: the
    /// blocking pool runs no worker.
    pub fn worker(self, worker: usize) -> Self {
        Self {
            worker: Some(worker),
            ..self
        }
    }

    /// Spawns as [`spawn`] does, on the named worker if one is set.
    ///
    /// # Errors
    ///
    /// When the default runtime or this thread's shard cannot start, and
    /// `InvalidInput` when the named worker does not exist or takes no new
    /// task.
    pub fn spawn<F>(self, future: F) -> io::Result<Task<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.named(|| {
            let inner = crate::rt::runtime::try_current()?;
            crate::rt::runtime::try_spawn(&inner, self.worker, future)
        })
    }

    /// Spawns as [`spawn_local`] does.
    ///
    /// # Errors
    ///
    /// When this thread's shard cannot start, and `InvalidInput` when a
    /// worker is named that this thread does not run: a future that is not
    /// `Send` cannot go to another thread.
    pub fn spawn_local<F>(self, future: F) -> io::Result<Task<F::Output>>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        self.named(|| crate::rt::runtime::try_spawn_local(self.worker, future))
    }

    /// Runs `f` on the blocking pool as [`spawn_blocking`] does.
    ///
    /// # Errors
    ///
    /// When the default runtime cannot start, and `InvalidInput` when a
    /// worker is named.
    pub fn spawn_blocking<F, T>(self, f: F) -> io::Result<Task<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.named(|| crate::rt::runtime::try_spawn_blocking(self.worker, f))
    }

    /// Spawns on the runtime `handle` refers to, as
    /// [`RuntimeHandle::spawn`](crate::runtime::RuntimeHandle::spawn) does,
    /// on the named worker if one is set.
    ///
    /// # Errors
    ///
    /// When this thread's shard cannot start (the runtime has no worker
    /// taking tasks), and `InvalidInput` when the named worker does not exist
    /// or takes no new task.
    pub fn spawn_on<F>(
        self,
        future: F,
        handle: &crate::runtime::RuntimeHandle,
    ) -> io::Result<Task<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.named(|| crate::rt::runtime::try_spawn(handle.inner(), self.worker, future))
    }

    /// Runs `spawn` with the builder's name waiting for the task it makes,
    /// then names the task if no hook's spawn did; the name is in place
    /// while this call holds the task's handle, so before the task can be
    /// destroyed.
    fn named<T>(self, spawn: impl FnOnce() -> io::Result<Task<T>>) -> io::Result<Task<T>> {
        let Some(name) = self.name else {
            return spawn();
        };
        let name: std::sync::Arc<str> = std::sync::Arc::from(name);
        let _ = NAMING.try_with(|naming| *naming.borrow_mut() = Some(std::sync::Arc::clone(&name)));
        let spawned = spawn();
        let waiting = NAMING
            .try_with(|naming| naming.borrow_mut().take())
            .ok()
            .flatten();
        if let (Ok(task), Some(name)) = (&spawned, waiting) {
            crate::rt::task::names::set(task.id(), &name);
        }
        spawned
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::rt::task::names;

    /// A name is readable while its task lives and gone once the task is:
    /// the table holds nothing for tasks that ended, however they ended.
    #[test]
    fn names_live_exactly_as_long_as_their_tasks() {
        crate::block_on(async {
            let done = Builder::new().name("done").spawn(async { 1 }).unwrap();
            let id = done.id();
            assert_eq!(names::get(id).as_deref(), Some("done"));
            assert_eq!(done.await, 1);

            let cancelled = Builder::new()
                .name("cancelled")
                .spawn(core::future::pending::<()>())
                .unwrap();
            cancelled.abort();
            assert!(cancelled.join().await.unwrap_err().is_cancelled());

            let blocking = Builder::new()
                .name("blocking")
                .spawn_blocking(|| 2)
                .unwrap();
            assert_eq!(blocking.await, 2);

            let unnamed = Builder::new().spawn(async { 3 }).unwrap();
            assert_eq!(names::get(unnamed.id()), None);
            assert_eq!(unnamed.await, 3);

            // Destroyed tasks have dropped their names (their last reference
            // may go on a worker a moment after the handle).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while names::count() != 0 && std::time::Instant::now() < deadline {
                yield_now().await;
            }
            assert_eq!(names::count(), 0, "a destroyed task kept its name");
            assert_eq!(names::get(id), None);
        });
    }
}
