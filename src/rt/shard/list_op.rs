//! An edit of a shard's list of tasks that a thread which lost the shard in a poll posts to the
//! shard's owner: only the owner edits the list, whose links have no atomics
//! (proofs/tla/listowner/ListOwner.tla). Each holds a reference to its task, so the task's
//! memory stays until the edit is applied.

use crate::rt::task::TaskRef;
use crate::rt::task::list::{Links, TaskList};

/// One posted edit.
pub(crate) enum ListOp {
    /// List the task, at its first poll that returned pending.
    List(TaskRef),
    /// Take the task off the list, as its future is dropped.
    Unlist(TaskRef),
}

impl ListOp {
    /// The edit that lists the task whose header holds `links`.
    ///
    /// # Safety
    ///
    /// `links` is the `links` of a live task's header.
    pub(crate) unsafe fn list(links: &Links) -> Self {
        // SAFETY: the caller's contract.
        Self::List(unsafe { TaskRef::new(links) })
    }

    /// The edit that unlists the task whose header holds `links`.
    ///
    /// # Safety
    ///
    /// As [`list`](Self::list).
    pub(crate) unsafe fn unlist(links: &Links) -> Self {
        // SAFETY: the caller's contract.
        Self::Unlist(unsafe { TaskRef::new(links) })
    }

    /// Applies the edit to `tasks`. A task completed or closed since is not listed: its future
    /// went, and a closed one is dropped by the owner of its runnable.
    ///
    /// # Safety
    ///
    /// The owner's thread.
    pub(crate) unsafe fn apply(self, tasks: &TaskList) {
        match self {
            Self::List(task) => {
                if task.is_open() && !task.links().is_listed() {
                    // SAFETY: the caller is the owner; the task is live and on no list.
                    unsafe { tasks.push(task.links()) };
                }
            }
            Self::Unlist(task) => {
                if task.links().is_listed() {
                    // SAFETY: the caller is the owner; the task is listed.
                    unsafe { task.links().unlink() };
                }
            }
        }
    }

    /// Applies the edit once the list was walked for the last time and holds no task: a task to
    /// list is cancelled at once, as the walk would have, and one to unlist is on no list.
    pub(crate) fn late(self) {
        if let Self::List(task) = self {
            drop(task.close());
        }
    }
}

#[cfg(test)]
impl ListOp {
    /// The task the edit holds, for a test.
    pub(crate) fn task(&self) -> &TaskRef {
        match self {
            Self::List(task) | Self::Unlist(task) => task,
        }
    }
}
