//! A source's operations on its shard's ring (io_uring): how a side's
//! [`Want`] becomes a ring entry, what each operation keeps while it is in
//! flight, and how its completion becomes an [`Outcome`].
//!
//! An operation is submitted on the ring of the source's shard, by that
//! ring's own thread (a side held elsewhere kicks it), and its entry names
//! the source's file by the ring's slot, never by the descriptor (see
//! `rt::shard::ring`). Everything the entry points at (the buffer, the
//! address, the message header) lives in the operation's [`RingMem`] until
//! its completion; a lent buffer moves into it and back.

use core::sync::atomic::Ordering;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use super::ops::{Answer, Dial, Done, Outcome, Want};
use super::{Addr, Kind, Source, Target};
use crate::buf::PoolBuf;
use crate::io::side::After;
use crate::io::sys;
use crate::rt::shard::ring::ShardRing;
use crate::syscore::linux::iouring::ops::NOT_AN_OP;
use crate::syscore::linux::iouring::ring::Sqe;

/// An operation of a source on its shard's ring.
pub(crate) struct RingOp {
    pub(crate) source: Arc<Source>,
    /// The slot of the source's file its entry names.
    pub(crate) file: u32,
    /// The side's epoch it was submitted in.
    pub(crate) epoch: u32,
    /// The read side's.
    pub(crate) read: bool,
    pub(crate) mem: RingMem,
}

/// What an operation's entry points at, kept until its completion.
pub(crate) enum RingMem {
    Ready,
    Accept(Box<RawAddr>),
    Recv(PoolBuf),
    RecvLent(Vec<u8>),
    RecvFrom(Box<Msg>, PoolBuf),
    Connect(
        Dial,
        #[expect(dead_code, reason = "the kernel reads it until the completion")] Box<RawAddr>,
    ),
    Send(PoolBuf),
    SendLent(Vec<u8>),
    SendTo(
        #[expect(dead_code, reason = "the kernel reads it until the completion")] Box<Msg>,
        PoolBuf,
        Target,
    ),
}

/// The kernel's answer `res` (a count, or a negated errno) to an operation
/// that takes bytes or gives a descriptor.
fn answer(res: i32) -> Answer<usize> {
    match res {
        r if r == -libc::ECANCELED => Answer::Undone { not_ready: false },
        r if r == -libc::EAGAIN => Answer::Undone { not_ready: true },
        r if r < 0 => Answer::Done(Err(io::Error::from_raw_os_error(-r))),
        r => Answer::Done(Ok(r as usize)),
    }
}

impl RingMem {
    /// The completion of an operation of a source of `kind`, whose kernel
    /// answer is `res`.
    pub(crate) fn complete(self, res: i32, kind: Kind) -> Outcome {
        match self {
            RingMem::Ready => Outcome::Ready(match answer(res) {
                Answer::Done(result) => Answer::Done(result.map(drop)),
                Answer::Undone { not_ready } => Answer::Undone { not_ready },
            }),
            RingMem::Accept(addr) => Outcome::Accept(match answer(res) {
                Answer::Done(Ok(fd)) => {
                    // SAFETY: the kernel made this descriptor for us.
                    let conn = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
                    Answer::Done(addr.to_addr(kind).map(|peer| (conn, peer)))
                }
                Answer::Done(Err(err)) => Answer::Done(Err(err)),
                Answer::Undone { not_ready } => Answer::Undone { not_ready },
            }),
            RingMem::Recv(mut buf) => {
                let answer = answer(res);
                if let Answer::Done(Ok(n)) = answer {
                    buf.set_filled(n);
                }
                Outcome::Recv(buf, answer)
            }
            RingMem::RecvLent(mut buf) => {
                let answer = answer(res);
                if let Answer::Done(Ok(n)) = answer {
                    // SAFETY: the kernel wrote the first `n` bytes of the
                    // buffer's spare capacity.
                    unsafe { buf.set_len(buf.len() + n) };
                }
                Outcome::RecvLent(buf, answer)
            }
            RingMem::RecvFrom(mut msg, mut buf) => {
                let answer = match answer(res) {
                    Answer::Done(Ok(n)) => {
                        buf.set_filled(n);
                        Answer::Done(msg.sender(kind).map(|from| (n, from)))
                    }
                    Answer::Done(Err(err)) => Answer::Done(Err(err)),
                    Answer::Undone { not_ready } => Answer::Undone { not_ready },
                };
                Outcome::RecvFrom(buf, answer)
            }
            RingMem::Connect(dial, _) => Outcome::Connect(
                dial,
                match res {
                    // Under way: its end is seen once the socket is ready.
                    r if r == -libc::EALREADY => Answer::Undone { not_ready: true },
                    r if r == -libc::EISCONN => Answer::Done(Ok(())),
                    r if r == -libc::ECANCELED => Answer::Undone { not_ready: false },
                    r if r < 0 => Answer::Done(Err(io::Error::from_raw_os_error(-r))),
                    _ => Answer::Done(Ok(())),
                },
            ),
            RingMem::Send(buf) => Outcome::Send(buf, answer(res)),
            RingMem::SendLent(buf) => Outcome::SendLent(buf, answer(res)),
            RingMem::SendTo(_, buf, target) => Outcome::SendTo(buf, target, answer(res)),
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

    fn of(dial: &Dial) -> io::Result<Box<Self>> {
        match dial {
            Dial::Inet(addr) => Ok(Self::inet(addr)),
            Dial::Unix(path) => Self::unix(path),
        }
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
// the buffer the operation holds with it; they move between threads only
// with the operation, which no two threads touch at once.
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

/// Submits a source's operations on a ring, from the ring's own thread.
pub(crate) struct RingIssuer<'a> {
    ring: &'a ShardRing,
    source: &'a Arc<Source>,
    file: u32,
}

impl<'a> RingIo<'a> {
    /// What submits operations from this thread; elsewhere the ring's own
    /// thread is kicked to submit what the side wants, and before the
    /// registration nothing is.
    pub(crate) fn issuer(self, after: &mut After) -> Option<RingIssuer<'a>> {
        match self {
            RingIo::Here { ring, source, file } => Some(RingIssuer { ring, source, file }),
            RingIo::Elsewhere { source } => {
                after.kick = source.home_held();
                None
            }
            RingIo::NotYet => None,
        }
    }
}

impl RingIssuer<'_> {
    /// Submits `want` for a side of a source of `kind` in `epoch`, and
    /// returns the operation's user data. A want no entry can carry (an
    /// address no socket can have) completes at once with the reason.
    pub(crate) fn issue(&self, want: Want, kind: Kind, epoch: u32, _after: &mut After) -> u64 {
        let read = want.of_read();
        match entry(want, kind, self.file as RawFd) {
            Ok((mem, sqe)) => self.ring.submit(
                RingOp {
                    source: Arc::clone(self.source),
                    file: self.file,
                    epoch,
                    read,
                    mem,
                },
                sqe,
            ),
            Err(outcome) => {
                self.source.op_out();
                self.source.op_done(Done {
                    outcome: *outcome,
                    epoch,
                    read,
                });
                NOT_AN_OP
            }
        }
    }
}

/// The entry for `want` on slot `slot`, with the memory it points at; or,
/// for an address no socket can have, the outcome that says so.
fn entry(want: Want, kind: Kind, slot: RawFd) -> Result<(RingMem, Sqe), Box<Outcome>> {
    Ok(match want {
        Want::Ready { read } => {
            let events = if read { libc::POLLIN } else { libc::POLLOUT };
            (RingMem::Ready, Sqe::poll_add(slot, events as u32))
        }
        Want::Accept => {
            let mut addr = RawAddr::empty();
            let sqe = Sqe::accept(
                slot,
                addr.as_mut_ptr(),
                &mut addr.len,
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            );
            (RingMem::Accept(addr), sqe)
        }
        Want::Recv(mut buf) => {
            let spare = buf.spare();
            let sqe = stream_read(kind, slot, spare.as_mut_ptr(), spare.len());
            (RingMem::Recv(buf), sqe)
        }
        Want::RecvLent(mut buf) => {
            let spare = buf.spare_capacity_mut();
            let sqe = stream_read(kind, slot, spare.as_mut_ptr().cast(), spare.len());
            (RingMem::RecvLent(buf), sqe)
        }
        Want::RecvFrom(mut buf) => {
            let spare = buf.spare();
            let mut msg = Msg::receiving(spare.as_mut_ptr(), spare.len());
            let sqe = Sqe::recvmsg(slot, &mut msg.hdr, 0);
            (RingMem::RecvFrom(msg, buf), sqe)
        }
        Want::Connect(dial) => match RawAddr::of(&dial) {
            Ok(addr) => {
                let sqe = Sqe::connect(slot, addr.as_ptr(), addr.len);
                (RingMem::Connect(dial, addr), sqe)
            }
            Err(err) => return Err(Box::new(Outcome::Connect(dial, Answer::Done(Err(err))))),
        },
        Want::Send(buf) => {
            let sqe = stream_write(kind, slot, buf.filled());
            (RingMem::Send(buf), sqe)
        }
        Want::SendLent(buf, from) => {
            let sqe = stream_write(kind, slot, buf.get(from..).unwrap_or_default());
            (RingMem::SendLent(buf), sqe)
        }
        Want::SendTo(buf, target) => {
            let to = match &target {
                Target::Connected => None,
                Target::Inet(addr) => Some(RawAddr::inet(addr)),
                Target::Unix(path) => match RawAddr::unix(path) {
                    Ok(addr) => Some(addr),
                    Err(err) => {
                        return Err(Box::new(Outcome::SendTo(
                            buf,
                            target,
                            Answer::Done(Err(err)),
                        )));
                    }
                },
            };
            let bytes = buf.filled();
            let msg = Msg::new(bytes.as_ptr().cast_mut(), bytes.len(), to);
            let sqe = Sqe::sendmsg(slot, &msg.hdr, sys::SEND_FLAGS as u32);
            (RingMem::SendTo(msg, buf, target), sqe)
        }
    })
}

/// A stream read of `len` bytes into `buf` on slot `slot`: `recv` on a
/// socket, `read` at the current position on a pipe or a terminal.
fn stream_read(kind: Kind, slot: RawFd, buf: *mut u8, len: usize) -> Sqe {
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    match kind {
        Kind::Bytes => Sqe::read(slot, buf, len, u64::MAX),
        _ => Sqe::recv(slot, buf, len, 0),
    }
}

/// A stream write of `bytes` on slot `slot`: `send` on a socket, `write` at
/// the current position on a pipe or a terminal.
fn stream_write(kind: Kind, slot: RawFd, bytes: &[u8]) -> Sqe {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    match kind {
        Kind::Bytes | Kind::File => Sqe::write(slot, bytes.as_ptr(), len, u64::MAX),
        _ => Sqe::send(slot, bytes.as_ptr(), len, sys::SEND_FLAGS),
    }
}

impl Source {
    /// Cancels the side's operation of `user_data`, on its ring's thread;
    /// the caller holds a side. A ring the source moved from cancelled it
    /// already, and a cancel that finds another operation by the same user
    /// data on the new one only has it asked again.
    pub(super) fn cancel_op(&self, user_data: u64) {
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

    /// The ring's thread submits what the source's sides want: a side held
    /// on another thread asked for it.
    pub(crate) fn ring_kick(self: &Arc<Self>) {
        let mut after = After::default();
        let hooks = self.hooks();
        self.read
            .defer(super::read_work::SUBMIT, &mut after, &hooks);
        self.write
            .defer(super::write_work::SUBMIT, &mut after, &hooks);
        self.settle(after);
    }

    /// The way this thread reaches a ring source's file, or `None` when
    /// the source is not registered with a ring.
    pub(super) fn route<'a>(
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
        fd: RawFd,
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
            None => match sys::duplicate(fd) {
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
    /// table, and has what the sides hold go to the ring.
    pub(crate) fn ring_attached(
        self: &Arc<Self>,
        ring: &ShardRing,
        shard: &Arc<crate::rt::shard::Shared>,
        copy: &OwnedFd,
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
        let mut after = After::default();
        let hooks = self.hooks();
        self.read
            .defer(super::read_work::ATTACHED, &mut after, &hooks);
        self.write
            .defer(super::write_work::ATTACHED, &mut after, &hooks);
        self.settle(after);
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
