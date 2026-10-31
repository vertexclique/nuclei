//! What a runtime notes around each task poll for its own use: the task
//! polled, in its runner's slot, so the monitor can name a poll that runs
//! past the handoff threshold (`Builder::on_long_poll`); and when the poll
//! began, so its time lands in its shard's histogram
//! (`Builder::record_latencies`).
//!
//! Both ride on the runtime's poll hooks, wrapped around the user's own
//! (which they leave out of the time), so a process where no runtime asks
//! for either pays nothing. Polls nest (a `block_on` inside a task polls
//! other tasks), so each poll's notes go on a per-thread stack: its end
//! puts back the task its beginning replaced, the one whose poll encloses
//! it (proofs/tla/longpoll).

use core::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

use super::context;
use super::shard::Shared;
use super::tuning::{TaskHooks, TaskMeta};

/// One poll on this thread's stack.
struct Note {
    /// The slot's task before this poll, put back at its end; `None` when
    /// nothing was noted.
    previous: Option<u64>,
    /// When the poll began, and the shard it began on.
    began: Option<(Instant, *const Shared)>,
}

std::thread_local! {
    static POLLS: RefCell<Vec<Note>> = const { RefCell::new(Vec::new()) };
}

/// Wraps the poll hooks of `hooks` so each poll notes its task
/// (`long_poll`) and its time (`latencies`); with neither, leaves them.
pub(crate) fn compose(hooks: &mut TaskHooks, long_poll: bool, latencies: bool) {
    if !long_poll && !latencies {
        return;
    }
    let (before, after) = (hooks.before_poll.take(), hooks.after_poll.take());
    hooks.before_poll = Some(Arc::new(move |meta: &TaskMeta<'_>| {
        if let Some(before) = &before {
            before(meta);
        }
        begin(meta.id().0.get(), long_poll, latencies);
    }));
    hooks.after_poll = Some(Arc::new(move |meta: &TaskMeta<'_>| {
        end();
        if let Some(after) = &after {
            after(meta);
        }
    }));
}

/// A poll of task `id` begins on this thread.
fn begin(id: u64, long_poll: bool, latencies: bool) {
    let previous = if long_poll {
        context::with_slot(|slot| slot.map(|slot| slot.note_task(id)))
    } else {
        None
    };
    let began = if latencies {
        context::current(|shard| shard.map(|s| (Instant::now(), Arc::as_ptr(&s.shared))))
    } else {
        None
    };
    let _ = POLLS.try_with(|polls| polls.borrow_mut().push(Note { previous, began }));
}

/// The poll [`begin`] began last on this thread ends.
fn end() {
    let Some(note) = POLLS
        .try_with(|polls| polls.borrow_mut().pop())
        .ok()
        .flatten()
    else {
        return;
    };
    if let Some(previous) = note.previous {
        // A runner whose shard was taken in this poll has no slot any more:
        // the one it noted in went with the shard's old runner.
        context::with_slot(|slot| slot.map(|slot| slot.note_task(previous)));
    }
    if let Some((began, shared)) = note.began {
        // SAFETY: the shard a poll began on outlives the poll: its loop, which
        // polls the task, holds it, and so does the monitor that takes it
        // from a runner blocked in this very poll.
        unsafe { &*shared }.counters.poll_time(began.elapsed());
    }
}
