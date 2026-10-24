//! A source's operations on its shard's ring (io_uring): what each keeps
//! while it is in flight, and how its completion reaches the side that
//! asked for it.
//!
//! An operation is submitted on the ring of the source's shard, under a
//! side of the source, and its entry names the source's file by the ring's
//! fixed slot, never by the descriptor (see `rt::shard::ring`). Everything
//! the entry points at (the buffer, the address, the message header) lives
//! in the operation's [`OpMem`] until its completion; a lent buffer moves
//! into it and back. A completion goes to its side through the side's
//! [`Mailbox`] and deferred word: applied at once if the side is free, else
//! by its holder as it lets go, so the ring never waits for a side.
//!
//! A side has one operation in flight at a time, so its mailbox holds one
//! completion at most.

use core::sync::atomic::{AtomicBool, Ordering};
use std::cell::UnsafeCell;
use std::io;
use std::sync::Arc;

use super::{Addr, Kind, Source};
use crate::buf::PoolBuf;
use crate::io::sys;
use crate::rt::shard::ring::ShardRing;

/// An operation of a source on its shard's ring.
pub(crate) struct RingOp {
    pub(crate) source: Arc<Source>,
    /// The slot of the source's file its entry names.
    pub(crate) file: u32,
    /// The side's epoch it was submitted in: a completion of an older one
    /// is of a descriptor that is gone, and is dropped.
    pub(crate) epoch: u32,
    pub(crate) mem: OpMem,
}

/// What an operation's entry points at, kept until its completion.
pub(crate) enum OpMem {
    /// A stream read into a buffer of the pool.
    Recv(PoolBuf),
    /// A stream read into a task's lent buffer, into its spare capacity.
    RecvLent(Vec<u8>),
    /// An accept, with room for the peer's address.
    Accept(Box<RawAddr>),
    /// A datagram received into a buffer of the pool, with its sender.
    RecvMsg(Box<Msg>, PoolBuf),
    /// Bytes of the write-behind being sent.
    Send(PoolBuf),
    /// A task's lent buffer being written, from the bytes the kernel
    /// took before on.
    SendLent(Vec<u8>),
    /// A datagram sent from a buffer of the pool, to its target.
    SendMsg(Box<Msg>, PoolBuf),
    /// A connect, to its peer.
    Connect(Box<RawAddr>),
    /// A wait for the socket to be ready, after an operation found it was
    /// not (on kernels that give that back instead of waiting).
    Poll,
}

impl OpMem {
    /// Lets the memory go: the pool's buffers go back through `after`.
    pub(crate) fn release(self, after: &mut crate::io::side::After) {
        match self {
            OpMem::Recv(buf)
            | OpMem::Send(buf)
            | OpMem::RecvMsg(_, buf)
            | OpMem::SendMsg(_, buf) => after.drop_buf(buf),
            _ => {}
        }
    }
}

/// A socket address in its raw form, as an entry points at it.
pub(crate) struct RawAddr {
    pub(crate) storage: libc::sockaddr_storage,
    pub(crate) len: libc::socklen_t,
}

impl RawAddr {
    /// Room for any address, for the kernel to fill.
    pub(crate) fn empty() -> Box<Self> {
        Box::new(Self {
            // SAFETY: plain data; all zeroes is a valid value.
            storage: unsafe { core::mem::zeroed() },
            len: size_of::<libc::sockaddr_storage>() as libc::socklen_t,
        })
    }

    pub(crate) fn inet(addr: &std::net::SocketAddr) -> Box<Self> {
        let (storage, len) = sys::raw_inet(addr);
        Box::new(Self { storage, len })
    }

    pub(crate) fn unix(path: &std::path::Path) -> io::Result<Box<Self>> {
        let (un, len) = sys::raw_unix(path)?;
        let mut raw = Self::empty();
        // SAFETY: `sockaddr_storage` is large and aligned enough for any
        // address; `un` is plain data.
        unsafe {
            core::ptr::write(
                (&mut raw.storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_un>(),
                un,
            )
        };
        raw.len = len;
        Ok(raw)
    }

    pub(crate) fn as_ptr(&self) -> *const libc::sockaddr {
        (&self.storage as *const libc::sockaddr_storage).cast()
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut libc::sockaddr {
        (&mut self.storage as *mut libc::sockaddr_storage).cast()
    }

    /// The address the kernel left, for a source of `kind`.
    pub(crate) fn to_addr(&self, kind: Kind) -> io::Result<Addr> {
        match kind {
            Kind::UnixListener | Kind::UnixDatagram | Kind::Stream => {
                // SAFETY: an `AF_UNIX` address fits `sockaddr_un`, which the
                // storage is at least as large as.
                let un = unsafe {
                    &*(&self.storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_un>()
                };
                Ok(Addr::Unix(sys::unix_addr(un, self.len)?))
            }
            _ => Ok(Addr::Inet(sys::inet_addr(&self.storage, self.len)?)),
        }
    }
}

/// A message header with its one buffer and its address, boxed so the
/// header's pointers into it stay put.
pub(crate) struct Msg {
    pub(crate) hdr: libc::msghdr,
    pub(crate) iov: libc::iovec,
    pub(crate) addr: Box<RawAddr>,
}

// SAFETY: the header's pointers point into the message's own box and into
// the buffer the operation holds with it; they move between threads only with
// the operation, which no two threads touch at once.
unsafe impl Send for Msg {}

impl Msg {
    /// A header over `buf` (`len` bytes at `ptr`) and `addr`; for a send
    /// to the connected peer, `addr` is `None`.
    pub(crate) fn new(ptr: *mut u8, len: usize, addr: Option<Box<RawAddr>>) -> Box<Self> {
        let named = addr.is_some();
        let mut msg = Box::new(Self {
            // SAFETY: plain data; all zeroes is a valid value.
            hdr: unsafe { core::mem::zeroed() },
            iov: libc::iovec {
                iov_base: ptr.cast(),
                iov_len: len,
            },
            addr: addr.unwrap_or_else(RawAddr::empty),
        });
        msg.hdr.msg_iov = &mut msg.iov;
        msg.hdr.msg_iovlen = 1 as _;
        if named {
            msg.hdr.msg_name = msg.addr.as_mut_ptr().cast();
            msg.hdr.msg_namelen = msg.addr.len;
        }
        msg
    }

    /// A header for a receive: room for the sender's address.
    pub(crate) fn receiving(ptr: *mut u8, len: usize) -> Box<Self> {
        let mut msg = Self::new(ptr, len, None);
        msg.hdr.msg_name = msg.addr.as_mut_ptr().cast();
        msg.hdr.msg_namelen = msg.addr.len;
        msg
    }

    /// The sender's address the kernel left.
    pub(crate) fn sender(&mut self, kind: Kind) -> io::Result<Addr> {
        self.addr.len = self.hdr.msg_namelen;
        self.addr.to_addr(kind)
    }
}

/// A completion for a side: the operation's memory and the kernel's result.
pub(crate) struct Done {
    pub(crate) mem: OpMem,
    /// A count or a descriptor on success, a negated errno on failure.
    pub(crate) res: i32,
    pub(crate) epoch: u32,
}

impl Done {
    /// The kernel gave the operation back undone: cancelled as its source
    /// went (but the source stayed, as a handle that lingers does), or the
    /// socket was not ready (a kernel that does not wait for it). It is
    /// asked again; a stream read or write made none of its bytes.
    pub(crate) fn undone(&self) -> bool {
        self.res == -libc::ECANCELED || self.res == -libc::EAGAIN
    }

    /// The socket was not ready: the next operation waits for it first.
    pub(crate) fn not_ready(&self) -> bool {
        self.res == -libc::EAGAIN
    }

    /// The result as a count, or the error.
    pub(crate) fn result(&self) -> io::Result<usize> {
        if self.res < 0 {
            Err(io::Error::from_raw_os_error(-self.res))
        } else {
            Ok(self.res as usize)
        }
    }
}

/// A side's completion, from the ring's thread to whoever holds the side
/// next. One at most: a side has one operation in flight.
#[derive(Default)]
pub(crate) struct Mailbox {
    full: AtomicBool,
    done: UnsafeCell<Option<Done>>,
}

// SAFETY: the completion is written only while `full` is clear, by the ring's
// thread, and taken only after `full` is seen set, by the side's holder.
unsafe impl Send for Mailbox {}
// SAFETY: as for `Send`.
unsafe impl Sync for Mailbox {}

impl Mailbox {
    /// Leaves `done` for the side.
    pub(crate) fn put(&self, done: Done) {
        // SAFETY: `full` is clear: no completion is out, and the side's one
        // operation completes once.
        unsafe { *self.done.get() = Some(done) };
        // Release: the holder reads the completion after it sees the flag.
        self.full.store(true, Ordering::Release);
    }

    /// The completion, if one waits.
    pub(crate) fn take(&self) -> Option<Done> {
        // Acquire: pairs with the Release of `put`.
        if !self.full.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `full` is set: the ring's thread is done with the slot.
        let done = unsafe { (*self.done.get()).take() };
        self.full.store(false, Ordering::Release);
        done
    }
}

/// The ring a source's operations go to, as the thread holding its side
/// sees it.
#[derive(Clone, Copy)]
pub(crate) enum RingIo<'a> {
    /// This thread runs the ring: operations are submitted here.
    Here {
        ring: &'a ShardRing,
        source: &'a Arc<Source>,
        file: u32,
    },
    /// Another thread runs it: that thread submits what the side needs.
    Elsewhere { source: &'a Arc<Source> },
    /// The registration with the ring is under way: the side waits, and is
    /// woken once it is in place.
    NotYet,
}

impl Source {
    /// The shard the source is registered with; the caller holds a side
    /// (the field is written only with both held).
    pub(crate) fn home_held(&self) -> Option<Arc<crate::rt::shard::Shared>> {
        // SAFETY: a side is held, so no writer (which holds both) is at work.
        unsafe { (*self.reg.shard.get()).clone() }
    }

    /// Runs `f` with the way this thread reaches the ring the source is
    /// registered with, if it is registered with one.
    pub(super) fn with_ring(self: &Arc<Self>, f: impl FnOnce(super::Io<'_>)) {
        crate::rt::context::current(|current| {
            if let Some(route) = self.ring_route(current) {
                f(super::Io::Ring(route));
            }
        });
    }

    /// One of the sides' operations goes to the ring.
    pub(crate) fn ring_op_out(&self) {
        self.ring_ops.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether none of the sides' operations is in the kernel's hands.
    pub(super) fn ring_idle(&self) -> bool {
        self.ring_ops.load(Ordering::SeqCst) == 0
    }

    /// Cancels the side's operation of `user_data`, on its ring's thread;
    /// the caller holds a side. A ring the source moved from cancelled it
    /// already, and a cancel that finds another operation by the same user
    /// data on the new one only has it asked again.
    pub(super) fn cancel_ring_op(&self, user_data: u64) {
        let Some(home) = self.home_held() else {
            return;
        };
        let here = crate::rt::context::here(|here| {
            here.filter(|s| s.is(&home))
                .and_then(|s| s.ring())
                .map(|ring| ring.cancel(user_data))
                .is_some()
        });
        if !here {
            home.post_io(crate::rt::shard::IoOp::RingCancel(user_data));
        }
    }

    /// The ring completed one of the source's operations: its side takes the
    /// completion at once if free, else its holder does as it lets go.
    pub(crate) fn ring_done(self: &Arc<Self>, done: Done) {
        // SeqCst, with the flag after it: against a retire that raises the
        // flag and then looks at the count (see `retire`).
        if self.ring_ops.fetch_sub(1, Ordering::SeqCst) == 1
            && self.ask.awaits_ops.load(Ordering::SeqCst)
            && let Some(waker) = self.ask.retire_waker.take()
        {
            waker.wake();
        }
        let mut after = crate::io::side::After::default();
        let hooks = self.hooks();
        match done.mem {
            OpMem::Recv(_) | OpMem::RecvLent(_) | OpMem::Accept(_) | OpMem::RecvMsg(..) => {
                self.read_done.put(done);
                self.read.defer(super::read_work::DONE, &mut after, &hooks);
            }
            _ => {
                self.write_done.put(done);
                self.write
                    .defer(super::write_work::DONE, &mut after, &hooks);
            }
        }
        self.settle(after);
    }

    /// The ring's thread submits what the source's sides want: a side held
    /// on another thread asked for it.
    pub(crate) fn ring_kick(self: &Arc<Self>) {
        let mut after = crate::io::side::After::default();
        let hooks = self.hooks();
        self.read
            .defer(super::read_work::SUBMIT, &mut after, &hooks);
        self.write
            .defer(super::write_work::SUBMIT, &mut after, &hooks);
        self.settle(after);
    }

    /// The way this thread reaches a ring source's file, or `None` when
    /// the source is not registered with a ring.
    pub(super) fn ring_route<'a>(
        self: &'a Arc<Self>,
        current: Option<&'a crate::rt::shard::Shard>,
    ) -> Option<RingIo<'a>> {
        if !self.reg.ring.load(Ordering::Acquire) {
            return None;
        }
        if self.reg.state.load(Ordering::Acquire) != super::REGISTERED {
            return Some(RingIo::NotYet);
        }
        let home = self.reg.home.load(Ordering::Acquire);
        let file = self.reg.slot.load(Ordering::Acquire);
        Some(
            match current.and_then(|s| s.ring().filter(|_| s.shared_addr() == home)) {
                Some(ring) => RingIo::Here {
                    ring,
                    source: self,
                    file,
                },
                None => RingIo::Elsewhere { source: self },
            },
        )
    }

    /// Registers the source with ring shard `shard` under `token`, with both
    /// sides held. On the ring's thread its file goes into the table now;
    /// elsewhere a copy of the descriptor goes to the ring's thread, which
    /// puts it there (only that thread may change the ring's table).
    pub(super) fn attach_ring(
        &self,
        shard: Arc<crate::rt::shard::Shared>,
        token: u64,
        fd: std::os::fd::RawFd,
    ) -> io::Result<()> {
        let here = crate::rt::context::here(|here| {
            here.filter(|s| s.is(&shard))
                .and_then(|s| s.ring().map(|ring| ring.attach(fd)))
        });
        self.reg.ring.store(true, Ordering::Release);
        self.reg
            .home
            .store(Arc::as_ptr(&shard) as usize, Ordering::Release);
        self.reg.token.store(token, Ordering::Release);
        match here {
            Some(Ok(slot)) => {
                self.reg.slot.store(slot, Ordering::Release);
                // SAFETY: both sides are held.
                unsafe { *self.reg.shard.get() = Some(shard) };
                self.reg.state.store(super::REGISTERED, Ordering::Release);
                Ok(())
            }
            Some(Err(err)) => self.ring_attach_failed(&shard, token, err),
            None => match crate::io::sys::duplicate(fd) {
                Ok(copy) => {
                    // SAFETY: both sides are held.
                    unsafe { *self.reg.shard.get() = Some(Arc::clone(&shard)) };
                    shard.post_io(crate::rt::shard::IoOp::RingAttach(token, copy));
                    Ok(())
                }
                Err(err) => self.ring_attach_failed(&shard, token, err),
            },
        }
    }

    fn ring_attach_failed(
        &self,
        shard: &Arc<crate::rt::shard::Shared>,
        token: u64,
        err: io::Error,
    ) -> io::Result<()> {
        self.reg.ring.store(false, Ordering::Release);
        self.reg.state.store(super::UNREGISTERED, Ordering::Release);
        shard.release_source(token);
        Err(err)
    }

    /// On the ring's thread: puts the file of `copy` (a copy of the
    /// source's descriptor made while the source could not close) into the
    /// table, and wakes the tasks that waited for the registration.
    pub(crate) fn ring_attached(
        self: &Arc<Self>,
        ring: &ShardRing,
        shard: &Arc<crate::rt::shard::Shared>,
        copy: &std::os::fd::OwnedFd,
        token: u64,
    ) {
        use std::os::fd::AsRawFd;
        // A source that closed or moved on meanwhile: its registration was
        // undone, and the copy only goes.
        if self.reg.state.load(Ordering::Acquire) != super::REGISTERING
            || self.reg.token.load(Ordering::Acquire) != token
        {
            return;
        }
        match ring.attach(copy.as_raw_fd()) {
            Ok(slot) => {
                self.reg.slot.store(slot, Ordering::Release);
                self.reg.state.store(super::REGISTERED, Ordering::Release);
            }
            Err(err) => {
                self.reg.ring.store(false, Ordering::Release);
                self.reg.state.store(super::UNREGISTERED, Ordering::Release);
                shard.release_source(token);
                self.registration_failed(&err);
                return;
            }
        }
        // What the sides hold goes to the ring.
        let mut after = crate::io::side::After::default();
        let hooks = self.hooks();
        self.read
            .defer(super::read_work::ATTACHED, &mut after, &hooks);
        self.write
            .defer(super::write_work::ATTACHED, &mut after, &hooks);
        self.settle(after);
    }

    /// The registration with a ring took, with both sides held: what the
    /// sides hold goes to the ring, as [`ring_attached`](Self::ring_attached)
    /// has it once the registration is in place.
    pub(super) fn ring_registered(
        self: &Arc<Self>,
        read: &mut super::read::ReadState,
        write: &mut super::write::WriteState,
        after: &mut crate::io::side::After,
    ) {
        self.with_ring(|io| {
            read.submit_wanted(io, self.kind, after);
            write.submit_wanted(io, self.kind, after);
        });
    }

    /// The source's registration with ring shard `shard` goes: its ring
    /// cancels the sides' operations `out` and lets go of `slot` once they
    /// are done, at once on the ring's thread, else from its mailbox.
    pub(super) fn detach_ring(
        &self,
        shard: &Arc<crate::rt::shard::Shared>,
        slot: u32,
        out: [Option<u64>; 2],
    ) {
        let done = crate::rt::context::here(|here| {
            here.filter(|s| s.is(shard))
                .and_then(|s| s.ring())
                .map(|ring| ring.detach(slot, out))
                .is_some()
        });
        if !done {
            shard.post_io(crate::rt::shard::IoOp::RingDetach(slot, out));
        }
    }
}
