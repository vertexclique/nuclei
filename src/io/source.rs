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
//! Every operation is completed by the driver, and every call on the
//! descriptor is the driver's, into runtime memory: a read lands in a
//! buffer of the pool and the task copies it out; a write's bytes are
//! copied into a buffer of the pool and the driver writes them out. A task
//! that asks hands the work to the driver, which makes the call at once if
//! the socket may be ready (on the task's thread, under the side's guard)
//! and otherwise on the socket's next edge; the task takes the result when
//! it is there and waits for a wake when it is not. No task waits for
//! readiness and then makes the call itself.
//!
//! # The descriptor
//!
//! The driver makes calls on the descriptor itself, so it must never touch
//! one the handle no longer owns (closed, or swapped behind `get_mut`). The
//! number lives in the source, and it changes, or the handle lets go of its
//! object, only through a request that needs both sides
//! ([`requests`]): whoever holds both performs it, so it
//! never overlaps a call of the driver, which holds a side while it makes
//! one. A dropped handle never waits for that: if a side is held, it hands
//! its object to the source and the last holder closes it. Retiring marks
//! the source stale and deregisters it before the descriptor can change;
//! the driver skips a stale source, and checks every event's token (slot
//! index and generation, never a pointer) against the registration in
//! place, so an event for one that is gone does nothing.

mod read;
pub(crate) mod requests;
#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
pub(crate) mod ring;
pub(crate) mod write;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::io;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::os::unix::net as unix;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Poll, Wake};

use self::read::ReadState;
use self::requests::Ask;
use self::write::WriteState;
use super::side::{After, Hooks, Side};
use crate::rt::context;
use crate::rt::park::Readiness;
use crate::rt::shard::{IoOp, Shared};

/// How the driver's part of a source reaches the descriptor.
#[derive(Clone, Copy)]
pub(crate) enum Io<'a> {
    /// Calls on the descriptor, made by the driver itself (epoll, kqueue).
    Fd(RawFd),
    /// Operations on the ring of the source's shard.
    #[cfg_attr(
        not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")),
        expect(dead_code, reason = "no ring where io_uring is not built")
    )]
    Ring(RingRef<'a>),
}

#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
pub(crate) type RingRef<'a> = ring::RingIo<'a>;
/// No ring where io_uring is not built: never made.
#[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")))]
pub(crate) type RingRef<'a> = core::marker::PhantomData<&'a ()>;

/// No descriptor: the source is retired.
const NO_FD: RawFd = -1;
/// No registration.
const NO_TOKEN: u64 = u64::MAX;

const UNREGISTERED: u8 = 0;
const REGISTERING: u8 = 1;
const REGISTERED: u8 = 2;

/// Side-local work of the read side (its deferred word).
mod read_work {
    /// Urgent data or a hangup was reported.
    pub(super) const UNSURE: u8 = 1;
    /// A buffer came back to the pool the side ran out of.
    pub(super) const UNSTARVE: u8 = 2;
    /// The future of a lent buffer went away.
    pub(super) const ABANDON: u8 = 4;
    /// The side's ring operation completed (its mailbox holds it).
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const DONE: u8 = 8;
    /// Submit what the side wants, on the ring's thread.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const SUBMIT: u8 = 16;
    /// The registration with the ring came into place.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const ATTACHED: u8 = 32;
}

/// Side-local work of the write side (its deferred word).
mod write_work {
    /// A buffer came back to the pool the side ran out of.
    pub(super) const UNSTARVE: u8 = 1;
    /// The future of a lent buffer went away.
    pub(super) const ABANDON: u8 = 2;
    /// The side's ring operation completed (its mailbox holds it).
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const DONE: u8 = 4;
    /// Submit what the side wants, on the ring's thread.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const SUBMIT: u8 = 8;
    /// The registration with the ring came into place.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    pub(super) const ATTACHED: u8 = 16;
}

/// What kind of descriptor a source is: which system calls fit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A connected Unix stream socket.
    Stream,
    /// A connected TCP stream socket.
    Tcp,
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
pub(crate) struct Failure {
    code: Option<i32>,
    kind: io::ErrorKind,
}

impl Failure {
    pub(crate) fn of(err: &io::Error) -> Self {
        Self {
            code: err.raw_os_error(),
            kind: err.kind(),
        }
    }

    pub(crate) fn error(self) -> io::Error {
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
    /// The shard that holds it. Written only with both sides held, so read
    /// with either.
    shard: UnsafeCell<Option<Arc<Shared>>>,
    /// The registration is with a ring: the source's operations are the
    /// ring's, on fixed slot `slot`, and run on the shard whose `Shared`
    /// is at `home` (an address, compared, never followed).
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    ring: AtomicBool,
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    slot: AtomicU32,
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    home: core::sync::atomic::AtomicUsize,
}

/// The part of a handle its shard's driver reaches.
pub(crate) struct Source {
    fd: AtomicI32,
    /// The descriptor a retired source had, to tell a revival with another
    /// one.
    retired_fd: AtomicI32,
    /// Moves on when a revival brings another descriptor: a side whose
    /// state is of an older one discards it when it is next taken.
    epoch: AtomicU32,
    /// The epoch a retire left, which one revival moves on.
    retired_epoch: AtomicU32,
    kind: Kind,
    read: Side<ReadState>,
    write: Side<WriteState>,
    reg: Registration,
    /// Work that needs both sides, and what it carries (see `requests`).
    requests: AtomicU8,
    ask: Ask,
    /// Whether the write side holds bytes or a datagram the kernel has not
    /// taken, as its last holder left it.
    unsent: AtomicBool,
    /// Completions of the sides' ring operations, for their holders.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    read_done: ring::Mailbox,
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    write_done: ring::Mailbox,
    /// The sides' operations in the kernel's hands: a retire waits until
    /// none is.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    ring_ops: core::sync::atomic::AtomicU8,
}

// SAFETY: the sides guard their contents; `reg.shard` is touched only with
// both sides held, and `ask` by its own protocol (see `requests`).
unsafe impl Send for Source {}
// SAFETY: as for `Send`.
unsafe impl Sync for Source {}

/// What a holder of one of the source's sides does for the work others
/// left it: the driver's part with the descriptor in place, side-local
/// work, the source's requests, and a side's catching up with a revival.
#[derive(Clone, Copy)]
struct SourceHooks<'a> {
    src: &'a Arc<Source>,
    /// For the driver's own edge: the registration the event came for.
    token: Option<u64>,
}

impl Hooks<ReadState> for SourceHooks<'_> {
    fn taken(&self, st: &mut ReadState, after: &mut After) {
        if self.src.catch_up(st.epoch_mut()) {
            st.discard(after);
        }
    }

    fn serve(&self, st: &mut ReadState, after: &mut After) {
        if let Some(fd) = self.src.serving_fd(self.token) {
            st.serve(fd, self.src.kind, true, after);
        }
    }

    fn deferred(&self, st: &mut ReadState, bits: u8, after: &mut After) {
        if bits & read_work::UNSURE != 0 {
            st.short_reads_unsure();
        }
        if bits & read_work::UNSTARVE != 0 {
            st.unstarve(after);
        }
        if bits & read_work::ABANDON != 0 {
            st.abandon_lent(after);
        }
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        {
            if bits & read_work::DONE != 0
                && let Some(done) = self.src.read_done.take()
            {
                st.ring_done(done, self.src.kind, after);
            }
            // After a completion, a kick or the registration, what the
            // side still wants goes to the ring: the tasks that wait are
            // woken by what it brings.
            if bits & (read_work::DONE | read_work::SUBMIT | read_work::ATTACHED) != 0 {
                self.src
                    .with_ring(|io| st.submit_wanted(io, self.src.kind, after));
            }
        }
    }

    fn requests(&self, st: &mut ReadState, after: &mut After) {
        self.src.requests_holding_read(st, after);
    }
}

impl Hooks<WriteState> for SourceHooks<'_> {
    fn taken(&self, st: &mut WriteState, after: &mut After) {
        if self.src.catch_up(st.epoch_mut()) {
            st.discard(after);
        }
    }

    fn serve(&self, st: &mut WriteState, after: &mut After) {
        if let Some(fd) = self.src.serving_fd(self.token) {
            st.serve(fd, self.src.kind, true, after);
        }
    }

    fn deferred(&self, st: &mut WriteState, bits: u8, after: &mut After) {
        if bits & write_work::UNSTARVE != 0 {
            st.unstarve(after);
        }
        if bits & write_work::ABANDON != 0 {
            st.abandon_lent(after);
            #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
            if let Some(user_data) = st.withdrawal() {
                self.src.cancel_ring_op(user_data);
            }
        }
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        {
            if bits & write_work::DONE != 0
                && let Some(done) = self.src.write_done.take()
            {
                st.ring_done(done, self.src.serving_fd(None), after);
            }
            if bits & (write_work::DONE | write_work::SUBMIT | write_work::ATTACHED) != 0 {
                self.src
                    .with_ring(|io| st.submit_wanted(io, self.src.kind, after));
            }
        }
    }

    fn released(&self, saw_requests: bool, after: &mut After) {
        if saw_requests {
            self.src.requests_after_write(after);
        }
    }

    fn releasing(&self, st: &mut WriteState) {
        // Release: a drop on another thread reads it.
        self.src.unsent.store(st.has_unsent(), Ordering::Release);
    }
}

impl Source {
    /// A source for `fd`, which the caller set non-blocking and keeps open
    /// for the source's life (until it is retired or closed).
    pub(crate) fn new(fd: RawFd, kind: Kind) -> Arc<Self> {
        Arc::new(Self {
            fd: AtomicI32::new(fd),
            retired_fd: AtomicI32::new(NO_FD),
            epoch: AtomicU32::new(0),
            retired_epoch: AtomicU32::new(0),
            kind,
            read: Side::new(ReadState::default()),
            write: Side::new(WriteState::default()),
            reg: Registration {
                state: AtomicU8::new(UNREGISTERED),
                token: AtomicU64::new(NO_TOKEN),
                shard: UnsafeCell::new(None),
                #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
                ring: AtomicBool::new(false),
                #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
                slot: AtomicU32::new(u32::MAX),
                #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
                home: core::sync::atomic::AtomicUsize::new(0),
            },
            requests: AtomicU8::new(0),
            ask: Ask::default(),
            unsent: AtomicBool::new(false),
            #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
            read_done: ring::Mailbox::default(),
            #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
            write_done: ring::Mailbox::default(),
            #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
            ring_ops: core::sync::atomic::AtomicU8::new(0),
        })
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// The hooks of a poll's or a request's hold.
    fn hooks(self: &Arc<Self>) -> SourceHooks<'_> {
        SourceHooks {
            src: self,
            token: None,
        }
    }

    /// The descriptor, unless the source is retired.
    pub(crate) fn raw_fd(&self) -> io::Result<RawFd> {
        self.fd()
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

    /// Runs `f` with the way this thread reaches the descriptor: calls on
    /// it (epoll, kqueue, and regular files everywhere), or the operations
    /// of the ring the source is registered with. A source about to
    /// register with a ring (this thread's shard runs one, or the
    /// process's shards do) registers before its first operation, which
    /// waits until the registration is in place.
    fn with_io<R>(self: &Arc<Self>, f: impl FnOnce(Io<'_>) -> R) -> io::Result<R> {
        let fd = self.fd()?;
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        if self.kind != Kind::File {
            return context::current(|current| {
                if !self.reg.ring.load(Ordering::Acquire) {
                    // Registered with a poller, or about to register where
                    // no ring runs: the driver's calls.
                    let rings =
                        current.map_or_else(crate::rt::backend::rings, |s| s.ring().is_some());
                    if !rings || self.reg.state.load(Ordering::Acquire) == REGISTERED {
                        return Ok(f(Io::Fd(fd)));
                    }
                    self.ensure_registered()?;
                }
                Ok(f(Io::Ring(
                    self.ring_route(current).unwrap_or(ring::RingIo::NotYet),
                )))
            });
        }
        Ok(f(Io::Fd(fd)))
    }

    /// No ring operation of the source can be in the kernel's hands.
    #[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")))]
    fn ring_idle(&self) -> bool {
        true
    }

    /// The descriptor the driver's part runs on, with a side held: for the
    /// driver's own edge, only while `token` is the registration in place.
    fn serving_fd(&self, token: Option<u64>) -> Option<RawFd> {
        if token.is_some_and(|t| self.reg.token.load(Ordering::Acquire) != t) {
            return None;
        }
        match self.fd.load(Ordering::Acquire) {
            NO_FD => None,
            fd => Some(fd),
        }
    }

    /// Whether a side last saw an older epoch than the source's: if so it
    /// catches up, and its state is of a descriptor that is gone.
    fn catch_up(&self, seen: &mut u32) -> bool {
        let now = self.epoch.load(Ordering::Acquire);
        if *seen == now {
            return false;
        }
        *seen = now;
        true
    }

    /// What every step leaves for once no guard is held: a side that ran out
    /// of buffers waits for one of this thread's pool, then the wakes and
    /// drops.
    fn settle(self: &Arc<Self>, mut after: After) {
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        if let Some(home) = after.kick.take() {
            home.post_io(crate::rt::shard::IoOp::RingKick(Arc::clone(self)));
        }
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

    /// Makes sure the driver of some shard watches the source: the current
    /// shard's, or (on a thread that runs none) one of the runtime's.
    fn ensure_registered(self: &Arc<Self>) -> io::Result<()> {
        // Registered, or being registered by another poll (readiness is
        // reported once the registration is in place). A shard that goes
        // evicts its sources first, so a registration in place has a driver.
        if self.reg.state.load(Ordering::Acquire) != UNREGISTERED {
            return Ok(());
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

    /// The shard could not even take the source in: the next wait tries
    /// again.
    pub(crate) fn registration_abandoned(&self) {
        self.reg.state.store(UNREGISTERED, Ordering::Release);
    }

    /// Deregisters the source; both sides are held by the caller, who
    /// hands their states in.
    #[cfg_attr(
        not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")),
        expect(unused_variables)
    )]
    fn detach(self: &Arc<Self>, read: &ReadState, write: &WriteState) {
        if self.reg.state.load(Ordering::Acquire) != REGISTERED {
            return;
        }
        // SAFETY: both sides are held.
        let shard = unsafe { (*self.reg.shard.get()).take() };
        let token = self.reg.token.swap(NO_TOKEN, Ordering::AcqRel);
        self.reg.state.store(UNREGISTERED, Ordering::Release);
        if let Some(shard) = shard {
            #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
            if self.reg.ring.swap(false, Ordering::AcqRel) {
                // The ring holds the file: the descriptor may close at once,
                // and the ring cancels what the sides have out.
                let slot = self.reg.slot.swap(u32::MAX, Ordering::AcqRel);
                self.detach_ring(&shard, slot, [read.ring_out(), write.ring_out()]);
                shard.release_source(token);
                return;
            }
            let fd = self.fd.load(Ordering::Acquire);
            if fd != NO_FD
                && let Some(registrar) = shard.registrar()
            {
                // Before the descriptor closes or changes hands. A failure
                // means the descriptor was already gone, and with it the
                // kernel's registration.
                let _ = registrar.delete(fd, self.kind == Kind::Tcp);
            }
            shard.release_source(token);
        }
    }

    /// A copy of the descriptor (the same open socket under another
    /// number), close-on-exec; `None` if the process has none to spare.
    pub(crate) fn duplicate(&self) -> Option<std::os::fd::OwnedFd> {
        super::sys::duplicate(self.fd().ok()?).ok()
    }

    /// Gives a retired source its descriptor again (the handle's IO object,
    /// possibly another one): the next wait registers it again. Another
    /// descriptor than before moves the epoch on, and each side discards
    /// what the old one left when it is next taken. Concurrent revivals of
    /// one object agree: the epoch moves on once, and the number is put in
    /// place once, after it.
    pub(crate) fn revive(&self, fd: RawFd) {
        if self.fd.load(Ordering::Acquire) != NO_FD {
            return;
        }
        if self.retired_fd.load(Ordering::Acquire) != fd {
            let retired = self.retired_epoch.load(Ordering::Acquire);
            let _ = self.epoch.compare_exchange(
                retired,
                retired.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        let _ = self
            .fd
            .compare_exchange(NO_FD, fd, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Whether the source is retired: the handle's next poll revives it.
    pub(crate) fn is_retired(&self) -> bool {
        self.fd.load(Ordering::Acquire) == NO_FD
    }

    /// Whether the write side holds bytes or a datagram the kernel has not
    /// taken yet, as its last holder left it.
    pub(crate) fn has_unsent(&self) -> bool {
        self.unsent.load(Ordering::Acquire)
    }

    /// The driver saw readiness for `token`.
    pub(crate) fn on_event(self: &Arc<Self>, token: u64, readiness: Readiness) {
        let mut after = After::default();
        let hooks = SourceHooks {
            src: self,
            token: Some(token),
        };
        if readiness.readable {
            let unsure = if readiness.urgent || readiness.hangup {
                read_work::UNSURE
            } else {
                0
            };
            self.read.drive(&mut after, &hooks, unsure);
        }
        if readiness.writable {
            self.write.drive(&mut after, &hooks, 0);
        }
        // If the driver's own pool ran out, the readers hear when a buffer
        // comes back and read for themselves.
        self.settle(after);
    }
}

#[cfg(test)]
impl Source {
    /// The read side as a line, for a test that failed: its guard's words,
    /// and what it holds unless it is held.
    fn read_report(self: &Arc<Self>) -> String {
        let (word, deferred) = self.read.words();
        let held = match self.read.try_hold() {
            Some(mut held) => {
                let line = held.get().report();
                let mut after = After::default();
                held.release(&mut after, &self.hooks());
                after.run();
                line
            }
            None => "held".into(),
        };
        format!("guard {word:#x}, deferred {deferred:#x}: {held}")
    }

    fn has_read_ahead(self: &Arc<Self>) -> bool {
        let mut after = After::default();
        let hooks = self.hooks();
        self.read.try_hold().is_some_and(|mut held| {
            let found = held.get().has_result();
            held.release(&mut after, &hooks);
            found
        })
    }

    fn has_write_behind(self: &Arc<Self>) -> bool {
        let mut after = After::default();
        let hooks = self.hooks();
        self.write.try_hold().is_some_and(|mut held| {
            let found = held.get().has_behind();
            held.release(&mut after, &hooks);
            found
        })
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
        let hooks = self.hooks();
        self.read.defer(read_work::UNSTARVE, &mut after, &hooks);
        self.write.defer(write_work::UNSTARVE, &mut after, &hooks);
        self.settle(after);
    }
}

#[cfg(test)]
mod tests;
