//! Tasks: spawning, their handles and ids, cancellation from anywhere, and
//! yielding to the other tasks of the shard.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

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
