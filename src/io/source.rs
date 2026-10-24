//! A source: the part of an IO handle its shard's driver reaches.
//!
//! One source per handle, made with it. It has two [`Side`]s, one for each
//! direction, each holding what the driver and the handle's polls hand each
//! other: the readiness cache, the tasks that wait, and the completed work
//! nobody took yet (bytes read ahead, accepted connections, datagrams) or
//! the work accepted and not done yet (bytes written behind, a queued
//! datagram, a connect in progress).
//!
//! # How an operation runs
//!
//! An operation first tries its system call at once, into the caller's own
//! buffer: most find the descriptor ready and finish there. One that would
//! block records that its task waits, and the driver, on the descriptor's
//! next readiness, makes the call itself on the shard thread into a buffer
//! of the shard's pool (a read) or from the bytes it was handed (a write),
//! and wakes the task, which takes the result. No task waits for readiness
//! and then makes the call: the driver completes the operation.
//!
//! # The descriptor
//!
//! The driver makes calls on the descriptor itself, so it must never touch
//! one the handle no longer owns (closed, or swapped behind `get_mut`). The
//! number lives in the source; a close or a `get_mut` retires the source
//! first: it takes both guards (waiting at most for one call of the
//! driver), marks the source stale and deregisters it before the descriptor
//! can change. The driver skips a stale source, and checks every event's
//! token (slot index and generation, never a pointer) against the
//! registration in place, so an event for one that is gone does nothing.

mod read;
pub(crate) mod write;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::io;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::os::unix::net as unix;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Poll, Wake};

use self::read::ReadState;
use self::write::WriteState;
use super::side::{After, Side};
use crate::rt::context;
use crate::rt::park::Readiness;
use crate::rt::shard::{IoOp, Shared};

/// No descriptor: the source is retired.
const NO_FD: RawFd = -1;
/// No registration.
const NO_TOKEN: u64 = u64::MAX;

const UNREGISTERED: u8 = 0;
const REGISTERING: u8 = 1;
const REGISTERED: u8 = 2;

/// What kind of descriptor a source is: which system calls fit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A connected stream socket (TCP or Unix).
    Stream,
    /// Any other byte stream (a pipe, a terminal).
    Bytes,
    /// A regular file or a block device: always ready to the kernel, so
    /// its IO goes to the blocking pool rather than to the poller.
    File,
    TcpListener,
    UnixListener,
    Udp,
    UnixDatagram,
}

impl Kind {
    fn is_datagram(self) -> bool {
        matches!(self, Kind::Udp | Kind::UnixDatagram)
    }
}

/// A peer's address.
#[derive(Clone, Debug)]
pub(crate) enum Addr {
    Inet(SocketAddr),
    Unix(unix::SocketAddr),
}

/// Where a datagram goes.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// The socket's connected peer.
    Connected,
    Inet(SocketAddr),
    Unix(PathBuf),
}

/// An error kept to be reported again: the kernel's code, or its kind.
#[derive(Clone, Copy, Debug)]
struct Failure {
    code: Option<i32>,
    kind: io::ErrorKind,
}

impl Failure {
    fn of(err: &io::Error) -> Self {
        Self {
            code: err.raw_os_error(),
            kind: err.kind(),
        }
    }

    fn error(self) -> io::Error {
        match self.code {
            Some(code) => io::Error::from_raw_os_error(code),
            None => self.kind.into(),
        }
    }
}

/// Where a source is registered.
struct Registration {
    state: AtomicU8,
    /// The token in place, or `NO_TOKEN`: what the driver checks an event
    /// against.
    token: AtomicU64,
    /// The shard that holds it. Written with both sides held, read with
    /// either.
    shard: UnsafeCell<Option<Arc<Shared>>>,
}

/// The part of a handle its shard's driver reaches.
pub(crate) struct Source {
    fd: AtomicI32,
    /// The descriptor a retired source had, to tell a revival with another
    /// one.
    retired_fd: AtomicI32,
    kind: Kind,
    read: Side<ReadState>,
    write: Side<WriteState>,
    reg: Registration,
}

// SAFETY: the sides guard their contents; `reg.shard` is written only with
// both sides held and read only with one.
unsafe impl Send for Source {}
// SAFETY: as for `Send`.
unsafe impl Sync for Source {}

impl Source {
    /// A source for `fd`, which the caller set non-blocking and keeps open
    /// for the source's life (until [`retire`](Self::retire)).
    pub(crate) fn new(fd: RawFd, kind: Kind) -> Arc<Self> {
        Arc::new(Self {
            fd: AtomicI32::new(fd),
            retired_fd: AtomicI32::new(NO_FD),
            kind,
            read: Side::new(ReadState::default()),
            write: Side::new(WriteState::default()),
            reg: Registration {
                state: AtomicU8::new(UNREGISTERED),
                token: AtomicU64::new(NO_TOKEN),
                shard: UnsafeCell::new(None),
            },
        })
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// The descriptor for a poll: the handle revived the source before it
    /// polled, so it is never stale here.
    fn fd(&self) -> io::Result<RawFd> {
        match self.fd.load(Ordering::Acquire) {
            NO_FD => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the handle's IO object is not in place",
            )),
            fd => Ok(fd),
        }
    }

    /// What every step leaves for once no guard is held: a side that ran out
    /// of buffers waits for one of this thread's pool, then the wakes and
    /// drops.
    fn settle(self: &Arc<Self>, mut after: After) {
        if after.starved {
            crate::buf::with_pool(|pool| {
                if let Some(pool) = pool {
                    pool.wait_for_return(Arc::clone(self).into());
                }
            });
        }
        after.run();
    }

    /// The end of a poll: settles, and makes sure a driver watches the
    /// source when the poll waits or left work to it.
    fn finish_poll<R>(
        self: &Arc<Self>,
        after: After,
        poll: Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let watch = after.register || poll.is_pending();
        self.settle(after);
        if watch && let Err(err) = self.ensure_registered() {
            return Poll::Ready(Err(err));
        }
        poll
    }

    /// The driver's part for a path that is not a poll: the descriptor is
    /// the handle's while it is not retired.
    fn serve_read(&self) -> impl FnMut(&mut ReadState, &mut After) + '_ {
        move |st, after| {
            if let Ok(fd) = self.fd() {
                st.serve(fd, self.kind, true, after);
            }
        }
    }

    fn serve_write(&self) -> impl FnMut(&mut WriteState, &mut After) + '_ {
        move |st, after| {
            if let Ok(fd) = self.fd() {
                st.serve(fd, self.kind, true, after);
            }
        }
    }

    /// Makes sure the driver of some shard watches the source: the current
    /// shard's, or (on a thread that runs none) one of the runtime's.
    fn ensure_registered(self: &Arc<Self>) -> io::Result<()> {
        match self.reg.state.load(Ordering::Acquire) {
            REGISTERED if !self.registered_on_stopped_shard() => return Ok(()),
            // The shard stopped with its runtime: one that runs takes over.
            REGISTERED => self.detach_held(),
            // Another poll is registering it; readiness is reported once the
            // registration is in place.
            REGISTERING => return Ok(()),
            _ => {}
        }
        if self
            .reg
            .state
            .compare_exchange(
                UNREGISTERED,
                REGISTERING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(());
        }
        match context::current(|current| current.map(|shard| shard.register_source(self))) {
            Some(result) => result,
            None => {
                crate::rt::runtime::service_shard().post_io(IoOp::Register(Arc::clone(self)));
                Ok(())
            }
        }
    }

    fn registered_on_stopped_shard(self: &Arc<Self>) -> bool {
        let mut after = After::default();
        let stopped = self.read.run(
            &mut after,
            // SAFETY: a side is held, so nobody writes the registration.
            |_, _| unsafe { (*self.reg.shard.get()).as_ref() }.is_some_and(|s| s.is_stopped()),
            self.serve_read(),
        );
        self.settle(after);
        stopped
    }

    /// Registers the source with `shard`'s poller under `token`. Called on
    /// that shard's thread by whoever moved the state to `REGISTERING`.
    pub(crate) fn attach(&self, shard: &Arc<Shared>, token: u64) -> io::Result<()> {
        let read = self.read.hold();
        let write = self.write.hold();
        let result = match self.fd.load(Ordering::Acquire) {
            NO_FD => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the handle closed before its registration",
            )),
            fd => shard.registrar().add(fd, token),
        };
        if result.is_ok() {
            // SAFETY: both sides are held.
            unsafe { *self.reg.shard.get() = Some(Arc::clone(shard)) };
            self.reg.token.store(token, Ordering::Release);
            self.reg.state.store(REGISTERED, Ordering::Release);
        } else {
            self.reg.state.store(UNREGISTERED, Ordering::Release);
        }
        // No registration was in place, so no driver flagged the sides.
        drop((write, read));
        result
    }

    /// The shard could not even take the source in: the next wait tries
    /// again.
    pub(crate) fn registration_abandoned(&self) {
        self.reg.state.store(UNREGISTERED, Ordering::Release);
    }

    /// A registration the shard made for a thread that runs none failed:
    /// the tasks waiting hear why, and the next wait tries again.
    pub(crate) fn registration_failed(&self, err: &io::Error) {
        let failure = Failure::of(err);
        let mut after = After::default();
        {
            let mut read = self.read.hold();
            read.get().fail(failure, &mut after);
        }
        {
            let mut write = self.write.hold();
            write.get().fail(failure, &mut after);
        }
        after.run();
    }

    /// Deregisters the source; both sides are held by the caller.
    fn detach(&self) {
        if self.reg.state.load(Ordering::Acquire) != REGISTERED {
            return;
        }
        // SAFETY: both sides are held.
        let shard = unsafe { (*self.reg.shard.get()).take() };
        let token = self.reg.token.swap(NO_TOKEN, Ordering::AcqRel);
        self.reg.state.store(UNREGISTERED, Ordering::Release);
        if let Some(shard) = shard {
            let fd = self.fd.load(Ordering::Acquire);
            if fd != NO_FD {
                // Before the descriptor closes or changes hands. A failure
                // means the descriptor was already gone, and with it the
                // kernel's registration.
                let _ = shard.registrar().delete(fd);
            }
            shard.release_source(token);
        }
    }

    fn detach_held(&self) {
        let read = self.read.hold();
        let write = self.write.hold();
        self.detach();
        drop((write, read));
    }

    /// Stops all IO on the descriptor and deregisters it, before the handle
    /// closes the descriptor or hands its IO object out mutably. Results
    /// nobody took yet stay for the next operation, unless a revival brings
    /// another descriptor.
    pub(crate) fn retire(&self) {
        let mut read = self.read.hold();
        let mut write = self.write.hold();
        self.detach();
        let fd = self.fd.swap(NO_FD, Ordering::AcqRel);
        if fd != NO_FD {
            self.retired_fd.store(fd, Ordering::Release);
        }
        // Only a handle held exclusively retires its source, so no task
        // borrows it: the wakers left are of operations that are gone.
        let mut after = After::default();
        read.get().forget_waiters(&mut after);
        write.get().forget_waiters(&mut after);
        // Edges flagged meanwhile are of the registration just removed.
        drop((write, read));
        after.run();
    }

    /// A copy of the descriptor (the same open socket under another
    /// number), close-on-exec; `None` if the process has none to spare.
    pub(crate) fn duplicate(&self) -> Option<std::os::fd::OwnedFd> {
        use std::os::fd::FromRawFd;
        let fd = self.fd().ok()?;
        // SAFETY: a plain fcntl on the handle's own open descriptor.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        // SAFETY: a new descriptor nobody else owns.
        (copy >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) })
    }

    /// The source goes on with `fd`, a copy of its descriptor the caller
    /// keeps open, as the handle's own closes: the registration under the
    /// old number goes first (while that number is still the handle's),
    /// reads stop, and what the write side holds is delivered through the
    /// copy.
    pub(crate) fn move_to(&self, fd: RawFd) {
        let mut read = self.read.hold();
        let mut write = self.write.hold();
        self.detach();
        self.fd.store(fd, Ordering::Release);
        let mut after = After::default();
        read.get().discard(&mut after);
        read.get().forget_waiters(&mut after);
        write.get().forget_waiters(&mut after);
        // Edges flagged meanwhile are of the registration just removed.
        drop((write, read));
        after.run();
    }

    /// Gives a retired source its descriptor again (the handle's IO object,
    /// possibly another one): the next wait registers it again. Another
    /// descriptor than before discards what the old one left.
    pub(crate) fn revive(&self, fd: RawFd) {
        if self.fd.load(Ordering::Acquire) != NO_FD {
            return;
        }
        if self.retired_fd.swap(NO_FD, Ordering::AcqRel) != fd {
            let mut after = After::default();
            self.read.hold().get().discard(&mut after);
            self.write.hold().get().discard(&mut after);
            after.run();
        }
        self.fd.store(fd, Ordering::Release);
    }

    /// Whether the source is retired: the handle's next poll revives it.
    pub(crate) fn is_retired(&self) -> bool {
        self.fd.load(Ordering::Acquire) == NO_FD
    }

    /// The driver saw readiness for `token`.
    pub(crate) fn on_event(self: &Arc<Self>, token: u64, readiness: Readiness) {
        let mut after = After::default();
        let kind = self.kind;
        if readiness.readable {
            self.read.drive(
                &mut after,
                |st, edge, after| {
                    if let Some(fd) = self.live_fd(token) {
                        st.serve(fd, kind, edge, after);
                    }
                },
                true,
            );
        }
        if readiness.writable {
            self.write.drive(
                &mut after,
                |st, edge, after| {
                    if let Some(fd) = self.live_fd(token) {
                        st.serve(fd, kind, edge, after);
                    }
                },
                true,
            );
        }
        // If the driver's own pool ran out, the readers hear when a buffer
        // comes back and read for themselves.
        self.settle(after);
    }

    /// The descriptor, if `token` is the registration in place and the
    /// source is not retired. Called with a side held.
    fn live_fd(&self, token: u64) -> Option<RawFd> {
        if self.reg.token.load(Ordering::Acquire) != token {
            return None;
        }
        match self.fd.load(Ordering::Acquire) {
            NO_FD => None,
            fd => Some(fd),
        }
    }
}

#[cfg(test)]
impl Source {
    fn has_read_ahead(&self) -> bool {
        self.read.hold().get().has_result()
    }

    fn has_write_behind(&self) -> bool {
        self.write.hold().get().has_behind()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // Whatever the sides still hold (read-ahead, accepted connections)
        // goes now; no guard is held, so the buffers may wake whoever waits.
        let mut after = After::default();
        self.read.get_mut().discard(&mut after);
        self.write.get_mut().discard(&mut after);
        after.run();
    }
}

/// A source woken as a buffer came back to a pool it ran out of: its tasks
/// waiting for a buffer try again.
impl Wake for Source {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut after = After::default();
        self.read.run(
            &mut after,
            |st, after| st.unstarve(after),
            self.serve_read(),
        );
        self.write.run(
            &mut after,
            |st, after| st.unstarve(after),
            self.serve_write(),
        );
        self.settle(after);
    }
}

#[cfg(test)]
mod tests;
