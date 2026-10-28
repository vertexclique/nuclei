//! The children whose exits a shard's driver watches (see `process::exit`):
//! by token in the shard's table on a poller (the child's pidfd in epoll,
//! its pid in kqueue's `EVFILT_PROC`) or a port (a wait packet on the
//! process handle), as operations on a ring. The driver reaps the child
//! when the exit comes and stores its outcome; a report with the child
//! still running (it cannot come, but costs nothing to take) watches it
//! again.

use std::sync::Arc;

use super::{IoOp, Shard, Shared};
use crate::process::exit::Exit;
use crate::rt::sources::{CHILD, Tokens};

/// A child the shard's poller or port watches.
pub(crate) struct Watched {
    exit: Arc<Exit>,
    /// The watch's wait packet.
    #[cfg(port)]
    packet: Packet,
}

/// A wait packet's handle.
#[cfg(port)]
struct Packet(crate::syscore::windows::HANDLE);

// SAFETY: a kernel object handle, used from any thread by design of the API.
#[cfg(port)]
unsafe impl Send for Packet {}

/// The children a shard's poller or port watches, by token.
pub(crate) type Children = Tokens<Watched, CHILD>;

impl Shard {
    /// Watches the exit of `exit`'s child from now on; a watch that cannot
    /// be made is the child's outcome.
    pub(crate) fn watch(&self, exit: Arc<Exit>) {
        #[cfg(ring)]
        if let Some(ring) = self.ring() {
            ring.watch_exit(exit);
            return;
        }
        let inserted = self.children.borrow_mut().insert(Watched {
            exit: Arc::clone(&exit),
            #[cfg(port)]
            packet: Packet(core::ptr::null_mut()),
        });
        let token = match inserted {
            Ok(token) => token,
            Err(err) => return exit.finish(Err(errno(&err))),
        };
        match self.register_exit(&exit, token) {
            // Exited already: its exit is taken now.
            Ok(false) => self.exit_came(token),
            Ok(true) => {}
            Err(err) => {
                self.children.borrow_mut().remove(token);
                exit.finish(Err(errno(&err)));
            }
        }
    }

    /// Has the driver report the exit of `exit`'s child with `token`;
    /// false when the child exited already.
    fn register_exit(&self, exit: &Exit, token: u64) -> std::io::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let (Some(registrar), Some(pidfd)) = (self.shared.registrar(), exit.pidfd()) else {
                return Err(std::io::ErrorKind::Unsupported.into());
            };
            registrar.add_exit(pidfd, token).map(|()| true)
        }
        #[cfg(all(readiness, not(any(target_os = "linux", target_os = "android"))))]
        {
            let Some(registrar) = self.shared.registrar() else {
                return Err(std::io::ErrorKind::Unsupported.into());
            };
            registrar.add_exit(exit.pid(), token)
        }
        #[cfg(port)]
        {
            let packet = self.shared.port().watch_exit(exit.process(), token)?;
            if let Some(watched) = self.children.borrow_mut().get_mut(token) {
                watched.packet = Packet(packet);
            }
            Ok(true)
        }
    }

    /// The driver reported the exit of the child of `token`.
    pub(super) fn exit_came(&self, token: u64) {
        let Some(watched) = self.children.borrow_mut().remove(token) else {
            return;
        };
        let exit = watched.exit;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let outcome = {
            // Before the pidfd can close: a one-shot registration stays.
            if let (Some(registrar), Some(pidfd)) = (self.shared.registrar(), exit.pidfd()) {
                let _ = registrar.delete(pidfd, false);
            }
            exit.reap_pidfd()
        };
        #[cfg(all(readiness, not(any(target_os = "linux", target_os = "android"))))]
        let outcome = exit.reap_by_pid();
        #[cfg(port)]
        let outcome = {
            crate::rt::park::iocp::Port::close_packet(watched.packet.0);
            crate::process::exit::windows_status(exit.process())
                .map_err(|err| errno(&err))
                .transpose()
        };
        match outcome {
            Some(outcome) => exit.finish(outcome),
            None => self.watch(exit),
        }
    }

    /// The shard stops running: each child it watches is let go once its
    /// registration ended (on a ring, at its cancelled watch's completion).
    pub(super) fn let_go_of_children(&self) {
        #[cfg(ring)]
        if let Some(ring) = self.ring() {
            ring.let_go_of_exits();
            return;
        }
        let gone: Vec<Watched> = self.children.borrow_mut().drain().collect();
        for watched in gone {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if let (Some(registrar), Some(pidfd)) = (self.shared.registrar(), watched.exit.pidfd())
            {
                let _ = registrar.delete(pidfd, false);
            }
            #[cfg(all(readiness, not(any(target_os = "linux", target_os = "android"))))]
            if let Some(registrar) = self.shared.registrar() {
                registrar.delete_exit(watched.exit.pid());
            }
            #[cfg(port)]
            crate::rt::park::iocp::Port::unwatch_exit(watched.packet.0);
            let orphan = Arc::strong_count(&watched.exit) == 1;
            watched.exit.unwatched(orphan);
        }
    }
}

fn errno(err: &std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(0)
}

/// Has the shard this thread runs watch `exit`'s child, or where no shard
/// runs, one that serves threads that run none.
pub(crate) fn watch_exit(exit: &Arc<Exit>) {
    let here = crate::rt::context::current(|current| {
        current.map(|shard| shard.watch(Arc::clone(exit))).is_some()
    });
    if !here {
        crate::rt::runtime::service_shard().post_io(IoOp::Watch(Arc::clone(exit)));
    }
}

/// Has a shard of a running runtime watch `exit`'s child, whose `Child`
/// is gone: whether one does. With no runtime running (the process is on
/// its way out) the child is left to the system, unreaped.
pub(crate) fn watch_exit_elsewhere(exit: &Arc<Exit>) -> bool {
    match crate::rt::runtime::running_service_shard() {
        Some(shard) => {
            Shared::post_io(&shard, IoOp::Watch(Arc::clone(exit)));
            true
        }
        None => false,
    }
}
