//! Task names, set by [`task::Builder`](crate::task::Builder) and read by the
//! task dump and tracing. A side table keyed by task id, touched only while
//! some task has a name: a task with none costs its destroy one load and no
//! memory.
//!
//! A name is stored while the builder still holds the task's handle, and a
//! task with a handle is never destroyed, so the destroy that removes the
//! entry always comes after the insert: no name outlives its task.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use super::Id;

/// Entries in the table.
static NAMED: AtomicUsize = AtomicUsize::new(0);

fn table() -> &'static kovan_map::HashMap<u64, Arc<str>> {
    static TABLE: OnceLock<kovan_map::HashMap<u64, Arc<str>>> = OnceLock::new();
    TABLE.get_or_init(|| kovan_map::HashMap::with_capacity(64))
}

/// Names the task `id`, whose handle the caller holds.
pub(crate) fn set(id: Id, name: &str) {
    NAMED.fetch_add(1, Ordering::Relaxed);
    if table().insert(id.0.get(), Arc::from(name)).is_some() {
        NAMED.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The name of the task `id`, if it has one.
#[allow(dead_code)] // read by the task dump
pub(crate) fn get(id: Id) -> Option<Arc<str>> {
    if NAMED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    table().get(&id.0.get())
}

/// Drops the name of the task `id`, destroyed now.
#[inline]
pub(super) fn forget(id: Id) {
    if NAMED.load(Ordering::Relaxed) != 0 {
        forget_named(id);
    }
}

#[inline(never)]
fn forget_named(id: Id) {
    if table().remove(&id.0.get()).is_some() {
        NAMED.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Named tasks not destroyed yet.
#[cfg(test)]
pub(crate) fn count() -> usize {
    NAMED.load(Ordering::Relaxed)
}
