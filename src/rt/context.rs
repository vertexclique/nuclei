//! What a thread knows about the runtime it runs: the shard it is running
//! now, its own shard (a thread that is not a worker gets one when it first
//! needs it), and the runtime `spawn` places tasks on.

use core::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use super::runtime::Inner;
use super::shard::{Limits, Shard};

/// The nesting a thread's own shard allows unless its runtime says
/// otherwise.
pub(crate) const DEFAULT_MAX_NESTING: usize = 256;

thread_local! {
    /// The shard this thread is running: a worker's, or the thread's own
    /// while it runs `block_on`.
    static CURRENT: Cell<*const Shard> = const { Cell::new(core::ptr::null()) };
    /// This thread's own shard, made on first need, dropped (with every task
    /// still queued on it) when the thread exits.
    static OWN: RefCell<Option<Rc<Shard>>> = const { RefCell::new(None) };
    /// The runtime `spawn` uses here: the worker's, or the one whose
    /// `block_on` (or `enter`) this thread is inside.
    static RUNTIME: RefCell<Option<Arc<Inner>>> = const { RefCell::new(None) };
}

/// Runs `f` with the shard this thread is running, if any.
#[inline]
pub(crate) fn current<R>(f: impl FnOnce(Option<&Shard>) -> R) -> R {
    let ptr = CURRENT.try_with(Cell::get).unwrap_or(core::ptr::null());
    // SAFETY: `CURRENT` points at a shard only while an `Entered` guard for
    // it lives, and the guard borrows the shard.
    f(unsafe { ptr.as_ref() })
}

/// Marks `shard` as the one this thread runs, until the guard drops.
pub(crate) fn enter(shard: &Shard) -> Entered<'_> {
    let previous = CURRENT.with(|c| c.replace(shard));
    Entered {
        previous,
        _shard: core::marker::PhantomData,
    }
}

/// Restores the shard that was current before [`enter`].
pub(crate) struct Entered<'a> {
    previous: *const Shard,
    _shard: core::marker::PhantomData<&'a Shard>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|c| c.set(self.previous));
    }
}

/// This thread's own shard, made now if it has none.
pub(crate) fn own_shard() -> io::Result<Rc<Shard>> {
    OWN.with(|own| {
        if let Some(shard) = own.borrow().as_ref() {
            return Ok(Rc::clone(shard));
        }
        let shard = Rc::new(Shard::new(Limits {
            max_nesting: DEFAULT_MAX_NESTING,
        })?);
        *own.borrow_mut() = Some(Rc::clone(&shard));
        Ok(shard)
    })
}

/// The runtime `spawn` uses on this thread, if one is set.
pub(crate) fn runtime() -> Option<Arc<Inner>> {
    RUNTIME
        .try_with(|r| r.borrow().as_ref().map(Arc::clone))
        .ok()
        .flatten()
}

/// Makes `inner` the runtime `spawn` uses on this thread, until the guard
/// drops.
pub(crate) fn set_runtime(inner: &Arc<Inner>) -> RuntimeSet {
    let previous = RUNTIME.with(|r| r.replace(Some(Arc::clone(inner))));
    RuntimeSet { previous }
}

/// Restores the runtime that was set before [`set_runtime`].
pub(crate) struct RuntimeSet {
    previous: Option<Arc<Inner>>,
}

impl Drop for RuntimeSet {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = RUNTIME.try_with(|r| *r.borrow_mut() = previous);
    }
}
