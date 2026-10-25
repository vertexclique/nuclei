//! A source's IO as operations a completion driver (an io_uring ring, an
//! I/O completion port) completes: what a side asks for, what comes back,
//! and how a completion reaches the side that asked.
//!
//! A side decides its next operation as a [`Want`], the memory it needs
//! moving with it (a buffer of the pool, a task's lent buffer, the address
//! of a datagram or a connect). The driver issues it and keeps that memory
//! until the kernel is done with it; its completion comes back as an
//! [`Outcome`], with the memory, through the side's [`Mailbox`] and
//! deferred word: applied at once if the side is free, else by its holder
//! as it lets go, so the driver never waits for a side. What a driver does
//! with a `Want`, and how it turns its own completion into an `Outcome`,
//! is the driver's (see `ring` and `port`); everything the side decides is
//! here and in the sides' `ops` modules, the same for both.
//!
//! A side has one operation out at a time, so its mailbox holds one
//! completion at most.

use core::sync::atomic::{AtomicBool, Ordering};
use std::cell::UnsafeCell;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use super::{Addr, Source, Target};
use crate::buf::PoolBuf;
use crate::io::raw::Owned;
use crate::io::side::After;

/// What a side asks its driver to do next.
pub(crate) enum Want {
    /// Wait until the socket may be read (`read`) or written: after a kernel
    /// gave an operation back because it was not, or for a connect under
    /// way.
    #[cfg(ring)]
    Ready { read: bool },
    /// Accept a connection.
    Accept,
    /// Accept connections as they come, until the accept is stopped: each
    /// is handed to the side as it comes, with no address (see
    /// [`Outcome::AcceptMultiEnd`]).
    #[cfg(ring)]
    AcceptMulti,
    /// Read a stream's bytes into the spare room of a buffer of the pool.
    Recv(PoolBuf),
    /// Receive a stream's bytes as they come, into buffers of the shard's
    /// receive group, until the receive is stopped: each arrival is handed
    /// to the side as it comes, apart from the end (see
    /// [`Outcome::RecvMultiEnd`]).
    #[cfg(ring)]
    RecvMulti,
    /// Read a stream's bytes into the spare capacity of a task's buffer.
    RecvLent(Vec<u8>),
    /// Receive a datagram into a buffer of the pool, with its sender.
    RecvFrom(PoolBuf),
    /// Connect to a peer.
    Connect(Dial),
    /// Send a stream the bytes a buffer of the pool holds.
    Send(PoolBuf),
    /// Send a stream a task's buffer, from `.1` on.
    SendLent(Vec<u8>, usize),
    /// Send a datagram from a buffer of the pool.
    SendTo(PoolBuf, Target),
}

impl Want {
    /// Whether the read side asked.
    pub(crate) fn of_read(&self) -> bool {
        match self {
            #[cfg(ring)]
            Want::Ready { read } => *read,
            Want::Accept | Want::Recv(_) | Want::RecvLent(_) | Want::RecvFrom(_) => true,
            #[cfg(ring)]
            Want::RecvMulti | Want::AcceptMulti => true,
            Want::Connect(_) | Want::Send(_) | Want::SendLent(..) | Want::SendTo(..) => false,
        }
    }

    /// Lets the memory go: the pool's buffers back through `after`.
    pub(crate) fn release(self, after: &mut After) {
        match self {
            Want::Recv(buf) | Want::RecvFrom(buf) | Want::Send(buf) | Want::SendTo(buf, _) => {
                after.drop_buf(buf)
            }
            _ => {}
        }
    }
}

/// The peer of a connect, owned while the connect is out.
#[derive(Clone, Debug)]
pub(crate) enum Dial {
    Inet(SocketAddr),
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}

/// The kernel's answer to an operation.
pub(crate) enum Answer<T> {
    /// It ran: its result.
    Done(io::Result<T>),
    /// It was given back undone, having taken none of its bytes: cancelled
    /// as its source moved or lingered on (`not_ready` false), or found the
    /// socket not ready on a kernel that gives that back instead of waiting.
    /// It is asked again.
    Undone { not_ready: bool },
}

/// What came back from an operation: the memory its `Want` gave the driver
/// (its bytes counted in), and the kernel's answer.
pub(crate) enum Outcome {
    #[cfg(ring)]
    Ready(Answer<()>),
    Accept(Answer<(Owned, Addr)>),
    /// A multishot accept ended: after its last connection (`Done(Ok(()))`,
    /// handed over already), an error, or a stop.
    #[cfg(ring)]
    AcceptMultiEnd(Answer<()>),
    Recv(PoolBuf, Answer<usize>),
    /// A multishot receive ended: on the stream's end (`Done(Ok(0))`), an
    /// error, a cancel, or after its last arrival (`Done(Ok(n))`, its bytes
    /// handed over already). `dry`: the receive group had no buffer left,
    /// so the next receive takes one of the pool.
    #[cfg(ring)]
    RecvMultiEnd {
        answer: Answer<usize>,
        dry: bool,
    },
    RecvLent(Vec<u8>, Answer<usize>),
    RecvFrom(PoolBuf, Answer<(usize, Addr)>),
    Connect(Dial, Answer<()>),
    Send(PoolBuf, Answer<usize>),
    SendLent(Vec<u8>, Answer<usize>),
    SendTo(PoolBuf, Target, Answer<usize>),
}

impl Outcome {
    /// Lets the memory go: the pool's buffers back through `after`.
    pub(crate) fn release(self, after: &mut After) {
        match self {
            Outcome::Recv(buf, _)
            | Outcome::RecvFrom(buf, _)
            | Outcome::Send(buf, _)
            | Outcome::SendTo(buf, ..) => after.drop_buf(buf),
            _ => {}
        }
    }
}

/// A completion for a side: what came back, and the side's epoch the
/// operation was issued in (one of an older epoch is of a descriptor that
/// is gone, and only its memory goes).
pub(crate) struct Done {
    pub(crate) outcome: Outcome,
    pub(crate) epoch: u32,
    /// The read side's.
    pub(crate) read: bool,
}

/// A side's completion, from the thread that dequeued it to whoever holds
/// the side next. One at most: a side has one operation out.
#[derive(Default)]
pub(crate) struct Mailbox {
    full: AtomicBool,
    done: UnsafeCell<Option<Done>>,
}

// SAFETY: the completion is written only while `full` is clear, by the
// thread that dequeued it, and taken only after `full` is seen set, by the
// side's holder.
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
        // SAFETY: `full` is set: the dequeuing thread is done with the slot.
        let done = unsafe { (*self.done.get()).take() };
        self.full.store(false, Ordering::Release);
        done
    }
}

impl Source {
    /// The shard the source is registered with; the caller holds a side
    /// (the field is written only with both held).
    pub(crate) fn home_held(&self) -> Option<Arc<crate::rt::shard::Shared>> {
        // SAFETY: a side is held, so no writer (which holds both) is at work.
        unsafe { (*self.reg.shard.get()).clone() }
    }

    /// Runs `f` with the way this thread reaches the driver the source is
    /// registered with, if it is registered with a completion driver.
    pub(super) fn with_route(self: &Arc<Self>, f: impl FnOnce(super::Io<'_>)) {
        crate::rt::context::current(|current| {
            if let Some(route) = self.route(current) {
                f(super::Io::Ops(route));
            }
        });
    }

    /// One of the sides' operations goes to the kernel.
    pub(crate) fn op_out(&self) {
        self.ops_out.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether none of the sides' operations is in the kernel's hands.
    pub(super) fn ops_idle(&self) -> bool {
        self.ops_out.load(Ordering::SeqCst) == 0
    }

    /// The kernel completed one of the source's operations: its side takes
    /// the completion at once if free, else its holder does as it lets go.
    pub(crate) fn op_done(self: &Arc<Self>, done: Done) {
        // SeqCst, with the flag after it: against a retire that raises the
        // flag and then looks at the count (see `retire`).
        if self.ops_out.fetch_sub(1, Ordering::SeqCst) == 1
            && self.ask.awaits_ops.load(Ordering::SeqCst)
            && let Some(waker) = self.ask.retire_waker.take()
        {
            waker.wake();
        }
        let mut after = After::default();
        let hooks = self.hooks();
        if done.read {
            self.read_done.put(done);
            self.read.defer(super::read_work::DONE, &mut after, &hooks);
        } else {
            self.write_done.put(done);
            self.write
                .defer(super::write_work::DONE, &mut after, &hooks);
        }
        self.settle(after);
    }

    /// The registration with a completion driver took, with both sides
    /// held: what the sides hold goes to it.
    pub(super) fn ops_registered(
        self: &Arc<Self>,
        read: &mut super::read::ReadState,
        write: &mut super::write::WriteState,
        after: &mut After,
    ) {
        self.with_route(|io| {
            read.submit_wanted(io, self.kind, after);
            write.submit_wanted(io, self.kind, after);
        });
    }
}
