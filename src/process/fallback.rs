//! The `SIGCHLD` reaper, for children Linux gave no pidfd (a kernel before
//! 5.3, or a filter that denies `pidfd_open`).
//!
//! Each delivery of `SIGCHLD` has every child the reaper holds reaped by
//! pid if it exited (the signal module's dispatch calls [`reap`] on a
//! pump). A child is held from the first wait on it (or from its `Child`
//! going) until it is reaped, so the reaper holds the children alive and
//! no more. The handler is installed before the first look at a child: an
//! exit before the look is seen by it, and one after raises `SIGCHLD`.

use std::sync::{Arc, OnceLock};

use super::exit::Exit;

/// The children the reaper holds, by pid.
fn children() -> &'static kovan_map::HashMap<u32, Arc<Exit>> {
    static CHILDREN: OnceLock<kovan_map::HashMap<u32, Arc<Exit>>> = OnceLock::new();
    // 64 is the smallest table the map accepts.
    CHILDREN.get_or_init(|| kovan_map::HashMap::with_capacity(64))
}

/// The reaper takes `exit`'s child.
pub(crate) fn watch(exit: &Arc<Exit>) {
    if let Err(err) = crate::signal::unix::listen_raw(libc::SIGCHLD) {
        exit.finish(Err(err.raw_os_error().unwrap_or(libc::EIO)));
        return;
    }
    children().insert(exit.pid(), Arc::clone(exit));
    crate::signal::unix::ensure_pump();
    reap();
}

/// Reaps every child held that exited.
pub(crate) fn reap() {
    for (pid, exit) in children().iter() {
        if let Some(outcome) = exit.reap_by_pid() {
            children().remove(&pid);
            exit.finish(outcome);
        }
    }
}
