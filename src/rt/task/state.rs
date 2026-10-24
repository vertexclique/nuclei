//! The bits of a task's state word.
//!
//! One `usize` holds every flag of a task and, above them, the count of its
//! references, so each transition of the task is one atomic operation on one
//! word. The flags take the low eight bits; [`REFERENCE`] is one reference.
//! The count tracks the [`Runnable`](super::Runnable) and every
//! [`Waker`](core::task::Waker); the [`Task`](super::Task) handle is tracked
//! by its own flag, [`TASK`].

/// The task has a `Runnable`: it is in a run queue, or about to be. Set
/// while it runs, it means the task was woken during its poll and runs
/// again after it. Never set once the task completed.
pub(super) const SCHEDULED: usize = 1 << 0;

/// The task's future is being polled.
pub(super) const RUNNING: usize = 1 << 1;

/// The future returned `Ready`: the cell holds the output (or the panic the
/// poll caught) until the task is closed.
pub(super) const COMPLETED: usize = 1 << 2;

/// The task is done with: cancelled, its output taken, or completed with no
/// handle left to take it. A closed task is never polled again.
pub(super) const CLOSED: usize = 1 << 3;

/// The `Task` handle exists.
pub(super) const TASK: usize = 1 << 4;

/// The handle registered a waker to be told of the completion.
pub(super) const AWAITER: usize = 1 << 5;

/// The handle is storing its waker.
pub(super) const REGISTERING: usize = 1 << 6;

/// Someone is taking the stored waker to wake it.
pub(super) const NOTIFYING: usize = 1 << 7;

/// One reference in the count above the flags.
pub(super) const REFERENCE: usize = 1 << 8;

/// The flag bits.
pub(super) const FLAGS: usize = REFERENCE - 1;
