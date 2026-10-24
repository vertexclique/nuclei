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
//! handle's close, with or without a hand-off to a copy of the descriptor
//! (the linger of bytes written behind). A dropped handle whose close cannot
//! run at once hands its IO object to the source: whoever performs the close
//! drops it, after the deregistration, so its descriptor closes only once
//! no call of the driver can use it.

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, Ordering};
use core::task::Poll;
use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;

use super::read::ReadState;
use super::write::WriteState;
use super::{Failure, Kind, NO_FD, NO_TOKEN, REGISTERED, Source, UNREGISTERED};
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
/// The handle is gone: deregister, and drop its object.
const CLOSE: u8 = 16;
/// The handle is gone with bytes still to send: deregister, go on with the
/// copy in `Ask::linger_fd`, and drop its object.
const LINGER: u8 = 32;

/// What pending requests carry. Each field is written by its requester
/// before the request's bit is raised (Release) and read by the performer
/// after the bit is taken (Acquire).
#[derive(Default)]
pub(crate) struct Ask {
    /// The shard and token of an attach.
    attach: AtomicPtr<(Arc<Shared>, u64)>,
    /// The failure of a registration.
    failure: AtomicPtr<Failure>,
    /// The copy of the descriptor a linger goes on with.
    linger_fd: AtomicI32,
    /// A dropped handle's IO object, for the close to drop.
    doomed: Doomed,
    /// The retire was performed; its requester waits on the waker.
    retired: AtomicBool,
    retire_waker: AtomicWaker,
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
    fn request(&self, bits: u8, after: &mut After) -> Done {
        // Release: the performer that takes the bits reads what they carry.
        self.requests.fetch_or(bits, Ordering::Release);
        let Some(mut read) = self.read.take_for_request() else {
            return Done {
                here: false,
                attached: None,
            };
        };
        let done = self.try_write_and_perform(read.get(), after, true);
        read.release(after, &self.hooks());
        done
    }

    /// For the read side's holder: takes the write side (one `fetch_or`
    /// that raises `REQ` there if it is held) and, getting it, performs the
    /// pending requests.
    fn try_write_and_perform(&self, read: &mut ReadState, after: &mut After, here: bool) -> Done {
        if self.requests.load(Ordering::Acquire) == 0 {
            return Done {
                here: false,
                attached: None,
            };
        }
        let Some(mut write) = self.write.take_for_request() else {
            return Done {
                here: false,
                attached: None,
            };
        };
        let attached = self.perform(read, write.get(), after, here);
        write.release(after, &self.hooks());
        Done {
            here: true,
            attached,
        }
    }

    /// For the read side's holder that saw `REQ`.
    pub(super) fn requests_holding_read(&self, read: &mut ReadState, after: &mut After) {
        self.try_write_and_perform(read, after, false);
    }

    /// For a holder of the write side that saw `REQ` and has let it go: one
    /// try at the read side (a holder of the write side never takes the
    /// read side while it holds its own, so two holders never pass the
    /// request back and forth); getting it, it goes on as the read side's
    /// holder.
    pub(super) fn requests_after_write(&self, after: &mut After) {
        if self.requests.load(Ordering::Acquire) == 0 {
            return;
        }
        if let Some(mut read) = self.read.take_for_request() {
            self.try_write_and_perform(read.get(), after, false);
            read.release(after, &self.hooks());
        }
    }

    /// Performs every pending request; both sides are held by the caller.
    /// Returns how an attach went, if one was pending: to the requester,
    /// when it is the one performing (`here`); else a failed attach fails
    /// the tasks waiting on the source, as a failed registration does.
    fn perform(
        &self,
        read: &mut ReadState,
        write: &mut WriteState,
        after: &mut After,
        here: bool,
    ) -> Option<io::Result<()>> {
        // Acquire: pairs with the Release of each requester.
        let bits = self.requests.swap(0, Ordering::Acquire);
        if bits & (RETIRE | CLOSE | LINGER) != 0 {
            self.detach();
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
            // left are of operations that are gone.
            read.forget_waiters(after);
            write.forget_waiters(after);
            if bits & RETIRE != 0 {
                self.ask.retired.store(true, Ordering::Release);
                if let Some(waker) = self.ask.retire_waker.take() {
                    after.wake_one(waker);
                }
            }
            if bits & (CLOSE | LINGER) != 0 {
                self.ask.doomed.closed();
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
        let attached = (bits & ATTACH != 0).then(|| self.attach_now());
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

    /// Stores `err` for the next performer of `FAIL` and raises it.
    fn leave_failure(&self, err: &io::Error) {
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
    fn attach_now(&self) -> io::Result<()> {
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
            fd => shard.registrar().add(fd, token, self.kind == Kind::Tcp),
        };
        match &result {
            Ok(()) => {
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
    fn evict_now(&self, read: &mut ReadState, write: &mut WriteState, after: &mut After) {
        if self.reg.state.load(Ordering::Acquire) == REGISTERED {
            // SAFETY: both sides are held.
            let shard = unsafe { (*self.reg.shard.get()).take() };
            self.reg.token.store(NO_TOKEN, Ordering::Release);
            self.reg.state.store(UNREGISTERED, Ordering::Release);
            if let (Some(shard), Ok(fd)) = (shard, self.fd()) {
                let _ = shard.registrar().delete(fd, self.kind == Kind::Tcp);
            }
        }
        read.wake_all(after);
        write.wake_all(after);
    }

    /// Registers the source with `shard`'s poller under `token`. Returns how
    /// it went when this thread could do it at once; when a side was held,
    /// its holder does it, and a failure reaches the tasks waiting on the
    /// source. Called on the shard's thread by whoever moved the state to
    /// registering.
    pub(crate) fn attach(&self, shard: &Arc<Shared>, token: u64) -> Option<io::Result<()>> {
        let ask = Box::into_raw(Box::new((Arc::clone(shard), token)));
        let old = self.ask.attach.swap(ask, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: made by `Box::into_raw`; swapped out, so ours alone.
            drop(unsafe { Box::from_raw(old) });
        }
        let mut after = After::default();
        let done = self.request(ATTACH, &mut after);
        after.run();
        done.attached.filter(|_| done.here)
    }

    /// A registration the shard made for a thread that runs none failed:
    /// the tasks waiting hear why, and the next wait tries again.
    pub(crate) fn registration_failed(&self, err: &io::Error) {
        self.leave_failure(err);
        let mut after = After::default();
        self.request(0, &mut after);
        after.run();
    }

    /// The shard the source is registered with is going away: the
    /// registration goes, and every task waiting on the source is woken, to
    /// register it where it polls next.
    pub(crate) fn evict(&self) {
        let mut after = After::default();
        self.request(EVICT, &mut after);
        after.run();
    }

    /// Stops all IO on the descriptor and deregisters it, before the handle
    /// hands its IO object out mutably or for good. Results nobody took yet
    /// stay for the next operation, unless a revival brings another
    /// descriptor. If a side is held, its holder performs the retire, and
    /// this waits for it (on a shard, its other tasks run meanwhile): the
    /// object must not change while a call of the driver may use it.
    pub(crate) fn retire(&self) {
        self.ask.retired.store(false, Ordering::Release);
        let mut after = After::default();
        let done = self.request(RETIRE, &mut after);
        after.run();
        if done.here {
            return;
        }
        let ask = &self.ask;
        crate::sync::internal::blocking(core::future::poll_fn(|cx| {
            ask.retire_waker.register(cx.waker());
            if ask.retired.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
    }

    /// The handle is gone: its registration goes and `io`, its IO object, is
    /// dropped (its descriptor closed) once no call of the driver can use
    /// it. With `linger`, a copy of the descriptor the caller keeps open, the
    /// source goes on with that copy to deliver what the write side holds.
    /// Never waits: if a side is held, the source keeps `io` and its last
    /// holder drops it.
    pub(crate) fn close<T>(&self, io: T, linger: Option<RawFd>) {
        let bit = match linger {
            Some(copy) => {
                self.ask.linger_fd.store(copy, Ordering::Release);
                LINGER
            }
            None => CLOSE,
        };
        let mut after = After::default();
        let done = self.request(bit, &mut after);
        after.run();
        if done.here {
            // Deregistered here: no call of the driver uses the number now.
            drop(io);
        } else {
            self.ask.doomed.hand_over(io);
        }
    }
}

/// A dropped handle's IO object, left to whoever performs the source's
/// close. The handle hands it over, and the performer marks the close done,
/// each with one swap; whichever comes second drops the object.
///
/// The object goes to another thread only for a source a driver or a poll
/// was using: IO exists only on std's sockets and files (see the handle's
/// docs), all of them `Send`, so the type-erased drop below is sound.
#[derive(Default)]
struct Doomed {
    state: AtomicU8,
    object: UnsafeCell<Option<Erased>>,
}

// The state starts at 0: nothing handed over, the close not performed.
const STORED: u8 = 1;
const CLOSED: u8 = 2;

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

// SAFETY: the object is touched by the handing thread before its swap and by
// the dropping thread after the other swap, never both; see the type's docs
// for why the object may be dropped on another thread.
unsafe impl Send for Doomed {}
// SAFETY: as for `Send`.
unsafe impl Sync for Doomed {}

impl Doomed {
    /// The handle hands its object over; dropped here if the close was
    /// performed already.
    fn hand_over<T>(&self, io: T) {
        let erased = Erased {
            ptr: NonNull::from(Box::leak(Box::new(io))).cast(),
            drop: drop_boxed::<T>,
        };
        // SAFETY: before the swap below the slot is this thread's alone.
        unsafe { *self.object.get() = Some(erased) };
        // AcqRel: publishes the object to the performer, or acquires the
        // performer's close.
        if self.state.swap(STORED, Ordering::AcqRel) == CLOSED {
            self.drop_object();
        }
    }

    /// The performer marks the close done; drops the object if the handle
    /// handed it over already.
    fn closed(&self) {
        if self.state.swap(CLOSED, Ordering::AcqRel) == STORED {
            self.drop_object();
        }
    }

    fn drop_object(&self) {
        // SAFETY: whichever swap came second reaches the slot, once.
        if let Some(erased) = unsafe { (*self.object.get()).take() } {
            // SAFETY: made from `Box::<T>::into_raw` with its own drop.
            unsafe { (erased.drop)(erased.ptr) };
        }
    }
}

impl Drop for Doomed {
    fn drop(&mut self) {
        self.drop_object();
    }
}
