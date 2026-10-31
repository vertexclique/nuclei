//! Requests: work on a source that needs both of its sides held, so that it
//! never overlaps a call of the driver on either.
//!
//! A request is a bit in the source's request word, with what it carries in
//! [`Ask`]. Sides are only ever taken in one order, the read side first, so
//! two holders never pass a request back and forth: performing takes the
//! write side while the read side is held, never the reverse. The requester
//! raises its bit and takes the read side with one `fetch_or` that also
//! raises `REQ`. Holding it, it takes the write side the same way; with both
//! it performs every pending request at once. When a side it needs is held,
//! it lets go of what it took and leaves the work to the holders, which
//! see `REQ` as they let go: a holder of the read side tries the write side
//! (one `fetch_or`, raising `REQ` there if it is held); a holder of the
//! write side lets go of it first, then tries the read side once, and if it
//! gets it, goes on as that side's holder. So whoever ends up holding both
//! performs, and nobody waits. `proofs/tla/side/Requests.tla` checks it,
//! including that a holder of the write side that takes the read side
//! while it holds its own can livelock.
//!
//! The requests: attaching the source to a shard's poller, a shard's evict,
//! a failed registration, a retire (`get_mut`, `into_inner`), and a dropped
//! handle's close, with or without a linger (the drain of bytes written
//! behind, on the handle's own object). A dropped handle hands its IO object
//! to the source ([`Keep`]): it is dropped once the close was performed,
//! after the deregistration, and no lease is out, so its descriptor closes
//! only once no call of the driver, no job of the blocking pool and no entry
//! queued on a ring can use it. A retire waits for the leases the same way
//! before the object changes hands.

use crate::io::raw::{AtomicRaw, Raw};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use core::task::Poll;
use std::io;
use std::sync::Arc;

#[cfg(readiness)]
use super::Kind;
use super::read::ReadState;
use super::write::WriteState;
use super::{Failure, NO_FD, NO_TOKEN, REGISTERED, Source, SourceHooks, UNREGISTERED};
use crate::io::side::After;
use crate::rt::shard::Shared;
use crate::sync::atomic_waker::AtomicWaker;

/// Register with the shard and token in `Ask::attach`.
const ATTACH: u8 = 1;
/// The shard the source is registered with is going away.
const EVICT: u8 = 2;
/// A registration failed with `Ask::failure`.
const FAIL: u8 = 4;
/// Stop all IO and deregister before the IO object changes hands.
const RETIRE: u8 = 8;
/// The handle is gone: deregister, and let go of its object.
const CLOSE: u8 = 16;
/// The handle is gone with bytes still to send: deregister, and go on with
/// the descriptor in `Ask::linger_fd` (the handle's own, which the linger
/// keeps open), for the drain to register again.
const LINGER: u8 = 32;
/// A ring put the file into its table for an attach performed on another
/// thread (`Ask::installs`): the registration takes it if it still wants
/// it, else the ring lets it go.
#[cfg(ring)]
const INSTALLED: u8 = 64;

/// What pending requests carry. Each field is written by its requester
/// before the request's bit is raised (Release) and read by the performer
/// after the bit is taken (Acquire).
pub(crate) struct Ask {
    /// The shard and token of an attach.
    attach: AtomicPtr<(Arc<Shared>, u64)>,
    /// The failure of a registration.
    failure: AtomicPtr<Failure>,
    /// The descriptor a linger goes on with.
    linger_fd: AtomicRaw,
    /// The handle's IO object once it dropped, and what keeps it open.
    pub(super) keep: Keep,
    /// The retire was performed; its requester waits on the waker.
    retired: AtomicBool,
    pub(super) retire_waker: AtomicWaker,
    /// A retire waits for the sides' ring operations to complete.
    pub(super) awaits_ops: AtomicBool,
    /// The files rings put into their tables for this source's attaches,
    /// a stack the ring threads push onto and the performer of
    /// `INSTALLED` empties.
    #[cfg(ring)]
    installs: AtomicPtr<Install>,
}

/// A file a ring put into its table (or could not) for the attach of
/// `token` that another thread performed.
#[cfg(ring)]
struct Install {
    token: u64,
    shard: Arc<Shared>,
    slot: io::Result<u32>,
    next: *mut Install,
}

impl Default for Ask {
    fn default() -> Self {
        Self {
            attach: AtomicPtr::default(),
            failure: AtomicPtr::default(),
            linger_fd: AtomicRaw::new(NO_FD),
            keep: Keep::new(),
            retired: AtomicBool::new(false),
            retire_waker: AtomicWaker::new(),
            awaits_ops: AtomicBool::new(false),
            #[cfg(ring)]
            installs: AtomicPtr::default(),
        }
    }
}

impl Drop for Ask {
    fn drop(&mut self) {
        let attach = core::mem::replace(self.attach.get_mut(), core::ptr::null_mut());
        if !attach.is_null() {
            // SAFETY: made by `Box::into_raw` and taken from the field once.
            drop(unsafe { Box::from_raw(attach) });
        }
        let failure = core::mem::replace(self.failure.get_mut(), core::ptr::null_mut());
        if !failure.is_null() {
            // SAFETY: as above.
            drop(unsafe { Box::from_raw(failure) });
        }
        // A file no performer took: its ring lets it go.
        #[cfg(ring)]
        {
            let mut next = core::mem::replace(self.installs.get_mut(), core::ptr::null_mut());
            while !next.is_null() {
                // SAFETY: made by `Box::into_raw` and pushed once.
                let install = *unsafe { Box::from_raw(next) };
                next = install.next;
                if let Ok(slot) = install.slot {
                    install.shard.post_io(crate::rt::shard::IoOp::RingDetach(
                        slot,
                        [None, None],
                        None,
                    ));
                }
            }
        }
    }
}

/// The outcome of a request: whether this thread performed it, and, for an
/// attach it performed, how it went.
pub(crate) struct Done {
    pub(crate) here: bool,
    pub(crate) attached: Option<io::Result<()>>,
}

impl Source {
    /// Raises `bits` and performs every pending request if both sides were
    /// free; else leaves them to the sides' holders. Only the read side is
    /// taken here: performing goes through it (see the module docs).
    fn request(self: &Arc<Self>, bits: u8, after: &mut After) -> Done {
        // The shard this thread runs is looked up inside the bracket that
        // keeps it in the shard, so a request from its thread is its owner's.
        crate::rt::context::current(|current| {
            self.request_with(bits, after, &self.hooks_in(current))
        })
    }

    fn request_with(
        self: &Arc<Self>,
        bits: u8,
        after: &mut After,
        hooks: &SourceHooks<'_>,
    ) -> Done {
        // Release: the performer that takes the bits reads what they carry.
        self.requests.fetch_or(bits, Ordering::Release);
        let Some(mut read) = self.read.take_for_request(hooks) else {
            return Done {
                here: false,
                attached: None,
            };
        };
        let done = self.try_write_and_perform(hooks, read.get(), after, true);
        read.release(after, hooks);
        done
    }

    /// For the read side's holder: takes the write side (one `fetch_or`
    /// that raises `REQ` there if it is held) and, getting it, performs the
    /// pending requests.
    fn try_write_and_perform(
        self: &Arc<Self>,
        hooks: &SourceHooks<'_>,
        read: &mut ReadState,
        after: &mut After,
        here: bool,
    ) -> Done {
        if self.requests.load(Ordering::Acquire) == 0 {
            return Done {
                here: false,
                attached: None,
            };
        }
        let Some(mut write) = self.write.take_for_request(hooks) else {
            return Done {
                here: false,
                attached: None,
            };
        };
        let attached = self.perform(read, write.get(), after, here);
        write.release(after, hooks);
        Done {
            here: true,
            attached,
        }
    }

    /// For the read side's holder that saw `REQ`.
    pub(super) fn requests_holding_read(
        self: &Arc<Self>,
        hooks: &SourceHooks<'_>,
        read: &mut ReadState,
        after: &mut After,
    ) {
        self.try_write_and_perform(hooks, read, after, false);
    }

    /// For a holder of the write side that saw `REQ` and has let it go: one
    /// try at the read side (a holder of the write side never takes the
    /// read side while it holds its own, so two holders never pass the
    /// request back and forth); getting it, it goes on as the read side's
    /// holder.
    pub(super) fn requests_after_write(
        self: &Arc<Self>,
        hooks: &SourceHooks<'_>,
        after: &mut After,
    ) {
        if self.requests.load(Ordering::Acquire) == 0 {
            return;
        }
        if let Some(mut read) = self.read.take_for_request(hooks) {
            self.try_write_and_perform(hooks, read.get(), after, false);
            read.release(after, hooks);
        }
    }

    /// Performs every pending request; both sides are held by the caller.
    /// Returns how an attach went, if one was pending: to the requester,
    /// when it is the one performing (`here`); else a failed attach fails
    /// the tasks waiting on the source, as a failed registration does.
    fn perform(
        self: &Arc<Self>,
        read: &mut ReadState,
        write: &mut WriteState,
        after: &mut After,
        here: bool,
    ) -> Option<io::Result<()>> {
        // Acquire: pairs with the Release of each requester.
        let bits = self.requests.swap(0, Ordering::Acquire);
        if bits & (RETIRE | CLOSE | LINGER) != 0 {
            self.detach(read, write);
            if bits & LINGER != 0 {
                // The registration under the old number went first, while
                // that number was still the handle's.
                self.fd.store(
                    self.ask.linger_fd.load(Ordering::Acquire),
                    Ordering::Release,
                );
                read.discard(after);
            } else {
                let fd = self.fd.swap(NO_FD, Ordering::AcqRel);
                if fd != NO_FD {
                    self.retired_fd.store(fd, Ordering::Release);
                    self.retired_epoch
                        .store(self.epoch.load(Ordering::Acquire), Ordering::Release);
                }
            }
            // No task borrows a handle that is retired or gone: the wakers
            // left are of operations that are gone, but for the drain of a
            // lingering close, which may wait already and goes on through
            // the copy.
            read.forget_waiters(after);
            if bits & LINGER != 0 {
                write.wake_all(after);
            } else {
                write.forget_waiters(after);
            }
            if bits & RETIRE != 0 {
                self.ask.retired.store(true, Ordering::Release);
                if let Some(waker) = self.ask.retire_waker.take() {
                    after.wake_one(waker);
                }
            }
            if bits & CLOSE != 0 {
                // The close's own hold on the object goes: the object is
                // dropped now, or by the last lease out.
                self.release_hold();
            }
        }
        if bits & FAIL != 0 {
            let failure = self
                .ask
                .failure
                .swap(core::ptr::null_mut(), Ordering::Acquire);
            if !failure.is_null() {
                // SAFETY: made by `Box::into_raw` and taken from the field
                // once, here.
                let failure = *unsafe { Box::from_raw(failure) };
                read.fail(failure, after);
                write.fail(failure, after);
            }
        }
        #[cfg(ring)]
        if bits & INSTALLED != 0 {
            self.take_installs(read, write, after);
        }
        let attached = (bits & ATTACH != 0).then(|| self.attach_now());
        #[cfg(completion)]
        if matches!(attached, Some(Ok(())))
            && self.reg.ops.load(Ordering::Acquire)
            && self.reg.state.load(Ordering::Acquire) == super::REGISTERED
        {
            // The driver took the source over here and now: tasks that
            // waited look again, and what the sides hold goes to it.
            self.ops_registered(read, write, after);
        }
        if !here && let Some(Err(err)) = &attached {
            let failure = Failure::of(err);
            read.fail(failure, after);
            write.fail(failure, after);
        }
        if bits & EVICT != 0 {
            self.evict_now(read, write, after);
        }
        attached
    }

    /// On a ring's thread: the ring put the file of the attach of `token`
    /// into its table under `slot` (or could not). Whether the registration
    /// still wants it is decided with both sides held, so a close, a retire
    /// or an evict meanwhile never leaves the file in the table.
    #[cfg(ring)]
    pub(crate) fn ring_installed(
        self: &Arc<Self>,
        shard: &Arc<Shared>,
        token: u64,
        slot: io::Result<u32>,
    ) {
        if slot.is_err() {
            shard.counters.refused_registration();
        }
        let install = Box::into_raw(Box::new(Install {
            token,
            shard: Arc::clone(shard),
            slot,
            next: core::ptr::null_mut(),
        }));
        let mut head = self.ask.installs.load(Ordering::Relaxed);
        loop {
            // SAFETY: the node is ours until the swap below publishes it.
            unsafe { (*install).next = head };
            // Release: the performer reads the node after it takes it.
            match self.ask.installs.compare_exchange_weak(
                head,
                install,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => head = now,
            }
        }
        let mut after = After::default();
        self.request(INSTALLED, &mut after);
        self.settle(after);
    }

    /// Takes the files the rings installed; both sides are held. The one of
    /// the attach still under way completes the registration; any other
    /// (the attach was undone, or the handle closed) goes back to its ring.
    #[cfg(ring)]
    fn take_installs(
        self: &Arc<Self>,
        read: &mut ReadState,
        write: &mut WriteState,
        after: &mut After,
    ) {
        // Acquire: pairs with the Release of each push.
        let mut next = self
            .ask
            .installs
            .swap(core::ptr::null_mut(), Ordering::Acquire);
        while !next.is_null() {
            // SAFETY: made by `Box::into_raw` and pushed once; the swap took it.
            let install = *unsafe { Box::from_raw(next) };
            next = install.next;
            // The attach under way, to this install's shard: one to another
            // shard may carry a token of the same number.
            let wanted = self.reg.state.load(Ordering::Acquire) == super::REGISTERING
                && self.reg.ops.load(Ordering::Acquire)
                && self.registered_with(Arc::as_ptr(&install.shard) as usize, install.token)
                && self.fd.load(Ordering::Acquire) != NO_FD;
            match (wanted, install.slot) {
                (true, Ok(slot)) => {
                    self.reg.slot.store(slot, Ordering::Release);
                    self.reg.state.store(REGISTERED, Ordering::Release);
                    // Tasks that waited look again, and what the sides hold
                    // goes to the ring.
                    self.ops_registered(read, write, after);
                }
                (true, Err(err)) => {
                    self.abandon_ring_attach();
                    let failure = Failure::of(&err);
                    read.fail(failure, after);
                    write.fail(failure, after);
                }
                (false, Ok(slot)) => self.detach_ring(&install.shard, slot, [None, None]),
                (false, Err(_)) => {}
            }
        }
    }

    /// Undoes an attach to a ring still under way on the ring's thread; both
    /// sides are held. The ring's install of it then finds it unwanted.
    #[cfg(ring)]
    pub(super) fn abandon_ring_attach(&self) {
        // SAFETY: both sides are held.
        let shard = unsafe { (*self.reg.shard.get()).take() };
        let token = self.reg.token.swap(NO_TOKEN, Ordering::AcqRel);
        self.reg.home.store(0, Ordering::Release);
        self.reg.ops.store(false, Ordering::Release);
        self.reg.state.store(UNREGISTERED, Ordering::Release);
        if let Some(shard) = shard {
            shard.release_source(token);
        }
    }

    /// Whether an attach to a ring is under way on the ring's thread: its
    /// file not taken yet.
    #[cfg(ring)]
    pub(super) fn ring_attach_under_way(&self) -> bool {
        self.reg.state.load(Ordering::Acquire) == super::REGISTERING
            && self.reg.ops.load(Ordering::Acquire)
    }

    /// Stores `err` for the next performer of `FAIL` and raises it.
    fn leave_failure(self: &Arc<Self>, err: &io::Error) {
        let failure = Box::into_raw(Box::new(Failure::of(err)));
        let old = self.ask.failure.swap(failure, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: made by `Box::into_raw`; swapped out, so ours alone.
            drop(unsafe { Box::from_raw(old) });
        }
        // Release: the performer reads the failure after taking the bit.
        self.requests.fetch_or(FAIL, Ordering::Release);
    }

    /// Registers the source with the shard and token the attach carries;
    /// both sides are held. A failure hands the slot back to the shard.
    fn attach_now(self: &Arc<Self>) -> io::Result<()> {
        let ask = self
            .ask
            .attach
            .swap(core::ptr::null_mut(), Ordering::Acquire);
        if ask.is_null() {
            return Ok(());
        }
        // SAFETY: made by `Box::into_raw` and taken from the field once.
        let (shard, token) = *unsafe { Box::from_raw(ask) };
        let result = match self.fd.load(Ordering::Acquire) {
            NO_FD => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the handle closed before its registration",
            )),
            _ if shard.is_stopped() => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the shard stopped before the registration",
            )),
            #[cfg(ring)]
            fd if shard.is_ring() => return self.attach_ring(shard, token, fd),
            #[cfg(port)]
            fd => return self.attach_port(shard, token, fd),
            #[cfg(readiness)]
            fd => match shard.registrar() {
                Some(registrar) => registrar
                    .add(fd, token, self.kind == Kind::Tcp)
                    .inspect_err(|_| shard.counters.refused_registration()),
                None => Err(io::Error::other("the shard has no poller to register with")),
            },
        };
        match &result {
            Ok(()) => {
                self.reg
                    .home
                    .store(Arc::as_ptr(&shard) as usize, Ordering::Release);
                // SAFETY: both sides are held.
                unsafe { *self.reg.shard.get() = Some(shard) };
                self.reg.token.store(token, Ordering::Release);
                self.reg.state.store(REGISTERED, Ordering::Release);
            }
            Err(_) => {
                self.reg.state.store(UNREGISTERED, Ordering::Release);
                shard.release_source(token);
            }
        }
        result
    }

    /// The registration goes, and every task waiting on the source is
    /// woken, to register it where it polls next; both sides are held.
    fn evict_now(
        self: &Arc<Self>,
        read: &mut ReadState,
        write: &mut WriteState,
        after: &mut After,
    ) {
        #[cfg(ring)]
        if self.ring_attach_under_way() {
            self.abandon_ring_attach();
        }
        if self.reg.state.load(Ordering::Acquire) == REGISTERED {
            // SAFETY: both sides are held.
            let shard = unsafe { (*self.reg.shard.get()).take() };
            self.reg.token.store(NO_TOKEN, Ordering::Release);
            self.reg.home.store(0, Ordering::Release);
            self.reg.state.store(UNREGISTERED, Ordering::Release);
            #[cfg(ring)]
            if self.reg.ops.swap(false, Ordering::AcqRel) {
                // What the sides have out is cancelled, and the ring lets go
                // of the file; the completions still reach the sides.
                if let Some(shard) = &shard {
                    let slot = self.reg.slot.swap(u32::MAX, Ordering::AcqRel);
                    self.detach_ring(shard, slot, [read.out(), write.out()]);
                }
            }
            #[cfg(port)]
            if self.reg.ops.swap(false, Ordering::AcqRel) {
                // What the sides have out is cancelled; its packets still
                // reach the sides, wherever the socket's completions go.
                self.detach_port();
            }
            #[cfg(readiness)]
            if let (Some(shard), Ok(fd)) = (shard, self.fd())
                && let Some(registrar) = shard.registrar()
            {
                let _ = registrar.delete(fd, self.kind == Kind::Tcp);
            }
            #[cfg(not(readiness))]
            drop(shard);
        }
        read.wake_all(after);
        write.wake_all(after);
    }

    /// Registers the source with `shard`'s poller under `token`. Returns how
    /// it went when this thread could do it at once; when a side was held,
    /// its holder does it, and a failure reaches the tasks waiting on the
    /// source. Called on the shard's thread by whoever moved the state to
    /// registering.
    pub(crate) fn attach(
        self: &Arc<Self>,
        shard: &Arc<Shared>,
        token: u64,
    ) -> Option<io::Result<()>> {
        let ask = Box::into_raw(Box::new((Arc::clone(shard), token)));
        let old = self.ask.attach.swap(ask, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: made by `Box::into_raw`; swapped out, so ours alone.
            drop(unsafe { Box::from_raw(old) });
        }
        let mut after = After::default();
        // Called on the shard's own thread, inside it: the owner of the
        // sides the registration will home there.
        let hooks = SourceHooks {
            claim: Some(shard),
            ..self.hooks()
        };
        let done = self.request_with(ATTACH, &mut after, &hooks);
        after.run();
        done.attached.filter(|_| done.here)
    }

    /// A registration the shard made for a thread that runs none failed:
    /// the tasks waiting hear why, and the next wait tries again.
    pub(crate) fn registration_failed(self: &Arc<Self>, err: &io::Error) {
        self.leave_failure(err);
        let mut after = After::default();
        self.request(0, &mut after);
        after.run();
    }

    /// The shard the source is registered with is going away: the
    /// registration goes, and every task waiting on the source is woken, to
    /// register it where it polls next.
    pub(crate) fn evict(self: &Arc<Self>) {
        let mut after = After::default();
        self.request(EVICT, &mut after);
        after.run();
    }

    /// Stops all IO on the descriptor and deregisters it, before the handle
    /// hands its IO object out mutably or for good. Results nobody took yet
    /// stay for the next operation, unless a revival brings another
    /// descriptor. If a side is held, its holder performs the retire, and
    /// this waits for it (on a shard, its other tasks run meanwhile): the
    /// object must not change while a call of the driver may use it. On a
    /// ring it waits, the same way, until the kernel is done with every
    /// operation of the source, so none takes bytes from the object after;
    /// and everywhere until no lease is out (a job of the blocking pool, an
    /// attach or a detach in a ring's mailbox), so none uses its number.
    pub(crate) fn retire(self: &Arc<Self>) {
        self.ask.retired.store(false, Ordering::Release);
        let mut after = After::default();
        let done = self.request(RETIRE, &mut after);
        after.run();
        if done.here && self.ops_idle() && !self.ask.keep.leased() {
            return;
        }
        let ask = &self.ask;
        crate::sync::internal::blocking(core::future::poll_fn(|cx| {
            ask.retire_waker.register(cx.waker());
            // SeqCst, after the waker is in: a completion that leaves no
            // operation out, or a lease that leaves none, either sees the
            // flag (and takes the waker) or its count is seen here.
            ask.awaits_ops.store(true, Ordering::SeqCst);
            if ask.retired.load(Ordering::Acquire) && self.ops_idle() && !ask.keep.leased() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        ask.awaits_ops.store(false, Ordering::SeqCst);
    }

    /// The handle is gone: its registration goes, and `io`, its IO object, is
    /// dropped (its descriptor closed) once the close was performed and no
    /// lease is out, so no call of the driver, no job of the blocking pool
    /// and no entry queued on a ring can use its number. Never waits: if a
    /// side is held, its holder performs the close; the last of the close
    /// and the leases drops the object.
    pub(crate) fn close<T>(self: &Arc<Self>, io: T) {
        // Before the request: whoever performs it may drop the object at once.
        self.ask.keep.hand_over(io);
        let mut after = After::default();
        self.request(CLOSE, &mut after);
        after.run();
    }

    /// The handle is gone with bytes still to send, which a task drains on
    /// `fd`, the handle's own descriptor, its object kept open meanwhile: the
    /// registration goes now (cancelling what is out, for the drain to
    /// register again where it runs), and the source goes on with `fd`.
    pub(crate) fn linger(self: &Arc<Self>, fd: Raw) {
        self.ask.linger_fd.store(fd, Ordering::Release);
        let mut after = After::default();
        self.request(LINGER, &mut after);
        after.run();
    }

    /// A lease on the handle's object, which keeps its descriptor open until
    /// it drops, for a call made on the number away from the driver (a job
    /// of the blocking pool, a ring's attach). Taken by a holder of the
    /// handle, or of another lease.
    ///
    /// # Errors
    ///
    /// The handle's IO object is not in place (retired).
    pub(crate) fn lease(self: &Arc<Self>) -> io::Result<Lease> {
        let raw = self.fd()?;
        Ok(self.hold(raw))
    }

    /// A lease that names `raw` (`NO_FD` for one that only holds).
    pub(super) fn hold(self: &Arc<Self>, raw: Raw) -> Lease {
        self.ask.keep.hold();
        Lease {
            source: Arc::clone(self),
            raw,
        }
    }

    /// A hold on the object goes: the last drops it, and a retire waiting
    /// for the leases looks again once one hold is left (the close's own: a
    /// retire comes only before the close).
    fn release_hold(&self) {
        match self.ask.keep.release() {
            Held::Dropped => {}
            Held::OneLeft => {
                if self.ask.awaits_ops.load(Ordering::SeqCst)
                    && let Some(waker) = self.ask.retire_waker.take()
                {
                    waker.wake();
                }
            }
            Held::More => {}
        }
    }
}

/// A hold on a handle's IO object: its descriptor stays open, and its number
/// the handle's, while the lease lives (see [`Source::lease`]).
pub(crate) struct Lease {
    source: Arc<Source>,
    raw: Raw,
}

impl Lease {
    /// The descriptor the lease keeps open.
    pub(crate) fn raw(&self) -> Raw {
        self.raw
    }

    /// The file of the descriptor, for a call of std's on it; the view never
    /// closes it.
    pub(crate) fn file(&self) -> core::mem::ManuallyDrop<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            // SAFETY: the lease keeps the descriptor open while the view
            // lives, and the view is never dropped as a file.
            core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(self.raw) })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            // SAFETY: as above, for the file's handle.
            core::mem::ManuallyDrop::new(unsafe {
                std::fs::File::from_raw_handle(self.raw as usize as std::os::windows::io::RawHandle)
            })
        }
    }
}

impl Clone for Lease {
    fn clone(&self) -> Self {
        self.source.hold(self.raw)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.source.release_hold();
    }
}

impl core::fmt::Debug for Lease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Lease").field("raw", &self.raw).finish()
    }
}

/// A handle's IO object once the handle dropped, and the holds that keep it
/// open: one for the close until it is performed, and one for every lease.
/// The handle hands the object over before it asks for the close; whichever
/// hold goes last drops it (proofs/tla/side/Requests.tla).
///
/// The object goes to another thread only for a source a driver, a poll or
/// a lease was using: IO exists only on std's sockets and files (see the
/// handle's docs), all of them `Send`, so the type-erased drop below is
/// sound.
pub(super) struct Keep {
    holds: AtomicUsize,
    object: UnsafeCell<Option<Erased>>,
}

/// What a hold that went left.
enum Held {
    /// It was the last: the object was dropped.
    Dropped,
    /// One hold is left: before a close, the close's own, so no lease is out.
    OneLeft,
    More,
}

/// A boxed object of a type the box no longer names, with its drop.
struct Erased {
    ptr: NonNull<()>,
    drop: unsafe fn(NonNull<()>),
}

/// Drops the `Box<T>` behind `ptr`.
///
/// # Safety
///
/// `ptr` came from `Box::<T>::into_raw` and is dropped once.
unsafe fn drop_boxed<T>(ptr: NonNull<()>) {
    // SAFETY: as the caller promises.
    drop(unsafe { Box::from_raw(ptr.cast::<T>().as_ptr()) });
}

// SAFETY: the object is written by the handing thread before its holds can
// all go, and taken by the thread whose release is the last, after every
// other release (AcqRel); see the type's docs for why it may be dropped on
// another thread.
unsafe impl Send for Keep {}
// SAFETY: as for `Send`.
unsafe impl Sync for Keep {}

impl Keep {
    /// The close's hold only.
    fn new() -> Self {
        Self {
            holds: AtomicUsize::new(1),
            object: UnsafeCell::new(None),
        }
    }

    /// The handle hands its object over, before it asks for the close (so
    /// the close's hold is still up).
    fn hand_over<T>(&self, io: T) {
        let erased = Erased {
            ptr: NonNull::from(Box::leak(Box::new(io))).cast(),
            drop: drop_boxed::<T>,
        };
        // SAFETY: the close's hold is up, so no release takes the slot yet.
        unsafe { *self.object.get() = Some(erased) };
    }

    /// A hold more, by a holder of the handle or of another hold.
    fn hold(&self) {
        self.holds.fetch_add(1, Ordering::SeqCst);
    }

    /// A hold goes; the last drops the object.
    fn release(&self) -> Held {
        // SeqCst: against a retire that raises its flag and then looks.
        match self.holds.fetch_sub(1, Ordering::SeqCst) {
            1 => {
                // SAFETY: the last hold: no other thread reaches the slot.
                if let Some(erased) = unsafe { (*self.object.get()).take() } {
                    // SAFETY: made from `Box::<T>::into_raw` with its own drop.
                    unsafe { (erased.drop)(erased.ptr) };
                }
                Held::Dropped
            }
            2 => Held::OneLeft,
            _ => Held::More,
        }
    }

    /// Whether a lease is out: a hold beside the close's.
    pub(super) fn leased(&self) -> bool {
        self.holds.load(Ordering::SeqCst) > 1
    }
}

impl Drop for Keep {
    fn drop(&mut self) {
        // A source dropped with its object still kept (no close was asked).
        if let Some(erased) = self.object.get_mut().take() {
            // SAFETY: made from `Box::<T>::into_raw` with its own drop.
            unsafe { (erased.drop)(erased.ptr) };
        }
    }
}
