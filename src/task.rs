//! Tasks: spawning (named or not), their handles and ids, cancellation from
//! anywhere, and yielding to the other tasks of the shard.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

pub use crate::rt::task::{AbortHandle, FallibleTask, Id, Join, JoinError, Task};
pub use crate::runtime::{spawn, spawn_blocking, spawn_local};
pub use crate::task_local::{AccessError, LocalKey, TaskLocalFuture};

/// The id of the task this code runs in.
///
/// # Panics
///
/// Outside a task: in `block_on`'s own future, or on a blocking-pool thread.
/// [`try_id`] answers `None` there instead.
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

/// Spawns tasks with options: for now a name, which the task dump and
/// tracing show.
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
}

impl<'a> Builder<'a> {
    /// A builder with no option set.
    pub fn new() -> Self {
        Self { name: None }
    }

    /// Names the task.
    pub fn name(self, name: &'a str) -> Self {
        Self { name: Some(name) }
    }

    /// Spawns as [`spawn`] does.
    pub fn spawn<F>(self, future: F) -> io::Result<Task<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(self.named(spawn(future)))
    }

    /// Spawns as [`spawn_local`] does.
    pub fn spawn_local<F>(self, future: F) -> io::Result<Task<F::Output>>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        Ok(self.named(spawn_local(future)))
    }

    /// Runs `f` on the blocking pool as [`spawn_blocking`] does.
    pub fn spawn_blocking<F, T>(self, f: F) -> io::Result<Task<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(self.named(spawn_blocking(f)))
    }

    /// Spawns on the runtime `handle` refers to, as
    /// [`RuntimeHandle::spawn`](crate::runtime::RuntimeHandle::spawn) does.
    pub fn spawn_on<F>(
        self,
        future: F,
        handle: &crate::runtime::RuntimeHandle,
    ) -> io::Result<Task<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(self.named(handle.spawn(future)))
    }

    /// Names `task` while this call holds its handle, so the name is in place
    /// before the task can be destroyed.
    fn named<T>(self, task: Task<T>) -> Task<T> {
        if let Some(name) = self.name {
            crate::rt::task::names::set(task.id(), name);
        }
        task
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
