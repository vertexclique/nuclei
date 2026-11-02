//! Panics the runtime catches in user code that no caller of the user's
//! polls: a task's future as it is dropped, an output nobody takes, a hook,
//! a `shard_local!` value as its worker stops. Each is caught where it
//! happens, so no change of the runtime's state is cut short, and goes to
//! the task's handle or, with nobody to take it, is logged at error
//! (`proofs/tla/task/Task.tla`, `proofs/tla/going/Locals.tla`).

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The payload of a caught panic.
pub(crate) type Panic = Box<dyn Any + Send + 'static>;

/// Runs `f`, and the panic it unwound with, if it did.
#[inline(always)]
pub(crate) fn catch(f: impl FnOnce()) -> Option<Panic> {
    catch_unwind(AssertUnwindSafe(f)).err()
}

/// The message of a panic whose payload is a string, as `panic!` makes it.
pub(crate) fn message(panic: &Panic) -> Option<&str> {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
}

/// Logs at error a caught panic nobody else is told of, `what` saying
/// where it came from, then drops it.
#[cold]
#[inline(never)]
pub(crate) fn lost(what: core::fmt::Arguments<'_>, panic: Panic) {
    match message(&panic) {
        Some(message) => log::error!("{what}: {message}"),
        None => log::error!("{what}"),
    }
    drop_payload(panic);
}

/// Drops a payload. One whose own drop panics is forgotten: the payload of
/// that panic could panic as it is dropped too.
pub(crate) fn drop_payload(panic: Panic) {
    if let Err(again) = catch_unwind(AssertUnwindSafe(|| drop(panic))) {
        core::mem::forget(again);
    }
}
