//! A stream write that lends the side its task's own buffer (see the module above): the
//! driver writes it out with no copy, and the task takes it back once the kernel took the last
//! byte. A lent buffer whose task went away is withdrawn if none of it went out, and finished by
//! the driver if some did.
//!
//! A zero-copy write lends its buffer the same way, and the ring sends its large chunks with
//! zero-copy sends: the first of those shares the buffer (`Pinned`), each send keeping a hold on
//! it until the kernel's notification. The write still ends for its task once the kernel took the
//! last byte, and the task gets the receiver the last hold hands the buffer back through.

use super::*;
use crate::buf::owned::OwnedBuf;

/// A task's own buffer, lent to the side for the driver to write out.
pub(super) struct Lent {
    pub(super) buf: OwnedBuf,
    /// Bytes the kernel took.
    pub(super) sent: usize,
    /// The write's result, once all of it went out or it failed.
    pub(super) done: Option<io::Result<usize>>,
    /// The task to wake when it is done; `None` while the task polls.
    pub(super) waker: Option<Waker>,
    /// The buffer is out with a ring operation (`buf` is empty meanwhile).
    pub(super) out: bool,
    /// The task went away after some of the buffer went out, or while it
    /// was out: the driver writes the rest, then drops it.
    // vertexia: the caller's allocation, kept uncharged by the pool (one per
    // source at most, for at most `LINGER` once its handle is gone); charge
    // it to the pool if kept buffers must count against the cap.
    pub(super) orphan: bool,
    /// Where the buffer goes, when it is one datagram: the write is whole or
    /// failed, never part.
    pub(super) to: Option<Target>,
    /// The range of a file sent in the kernel, with `buf` empty.
    #[cfg(windows)]
    pub(super) file: Option<FileRange>,
    /// The task asked for zero-copy sends, which the ring makes of large chunks.
    #[cfg(ring)]
    pub(super) zero_copy: bool,
    /// Once a send of it went out zero-copy: the buffer, shared with the sends the kernel may
    /// still read it for (`buf` is empty), and the receiver that hands it back to the task.
    #[cfg(ring)]
    pub(super) pinned: Option<(Pinned, oneshot::Receiver<OwnedBuf>)>,
}

/// A lent write's buffer on its task's side: the buffer while the task has it, and once a
/// zero-copy send shares it, the receiver it comes back through.
#[derive(Default)]
pub(crate) struct Held {
    pub(crate) buf: Option<OwnedBuf>,
    #[cfg(ring)]
    back: Option<oneshot::Receiver<OwnedBuf>>,
}

impl Held {
    pub(crate) fn of(buf: OwnedBuf) -> Self {
        Self {
            buf: Some(buf),
            #[cfg(ring)]
            back: None,
        }
    }

    /// The task holds nothing of it: the buffer is lent to the side.
    pub(crate) fn lent(&self) -> bool {
        #[cfg(ring)]
        if self.back.is_some() {
            return false;
        }
        self.buf.is_none()
    }

    /// What a zero-copy write gives its task once it ended: the buffer, or what hands it back.
    #[cfg(ring)]
    pub(crate) fn released<B>(&mut self) -> crate::net::Released<B> {
        if let Some(back) = self.back.take() {
            return crate::net::Released::waiting(back);
        }
        crate::net::Released::ready(self.buf.take().unwrap_or_default())
    }
}

impl Lent {
    /// The bytes the write is for: the buffer's, or a file range's.
    #[cfg(completion)]
    pub(super) fn total(&self) -> usize {
        #[cfg(windows)]
        if let Some(file) = &self.file {
            return file.len as usize;
        }
        self.len()
    }

    /// How many bytes the buffer holds, wherever it is.
    pub(super) fn len(&self) -> usize {
        #[cfg(ring)]
        if let Some((pinned, _)) = &self.pinned {
            return pinned.bytes().len();
        }
        self.buf.len()
    }

    /// The buffer's bytes not sent yet, wherever the buffer is: for a
    /// vectored write, the rest of the buffer the next byte is in.
    #[cfg(readiness)]
    fn rest(&self) -> &[u8] {
        #[cfg(ring)]
        if let Some((pinned, _)) = &self.pinned {
            return pinned.bytes().get(self.sent..).unwrap_or_default();
        }
        self.buf.rest_from(self.sent)
    }

    /// The task's side of it, as the write ends or the task takes it back: the buffer, or once a
    /// zero-copy send shares it, the receiver it comes back through (this hold goes now).
    fn into_held(self) -> Held {
        #[cfg(ring)]
        if let Some((_, back)) = self.pinned {
            return Held {
                buf: None,
                back: Some(back),
            };
        }
        Held::of(self.buf)
    }
}

impl WriteState {
    #[cfg(readiness)]
    /// Writes the lent buffer out, after the write-behind. Returns whether
    /// anything changed.
    pub(super) fn push_lent(&mut self, fd: RawFd, kind: Kind, after: &mut After) -> bool {
        let Some(lent) = self.lent.as_mut() else {
            return false;
        };
        let mut progress = false;
        if let Some(failure) = self.failed {
            // The write-behind before it failed: none of it went out.
            lent.done = Some(Err(failure.error()));
        }
        if let Some(to) = lent
            .to
            .as_ref()
            .filter(|_| lent.done.is_none() && !self.blocked)
        {
            match sys::ready(send_datagram(fd, &lent.buf, to.as_dest())) {
                None => self.blocked = true,
                Some(result) => {
                    lent.done = Some(result);
                    progress = true;
                }
            }
        }
        while lent.done.is_none() && !self.blocked {
            match sys::ready(write_bytes(fd, kind, lent.rest())) {
                None => self.blocked = true,
                Some(Ok(0)) => lent.done = Some(Err(io::ErrorKind::WriteZero.into())),
                Some(Ok(n)) => {
                    lent.sent += n;
                    progress = true;
                    let len = lent.len();
                    if lent.sent >= len {
                        lent.done = Some(Ok(len));
                    }
                }
                Some(Err(err)) => lent.done = Some(Err(err)),
            }
        }
        if lent.done.is_none() {
            return progress;
        }
        self.lent_finished(after);
        true
    }

    /// The lent buffer's write ended: its task is woken, and one whose task
    /// went is dropped.
    pub(super) fn lent_finished(&mut self, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        if let Some(w) = lent.waker.take() {
            after.wake_one(w);
        }
        if lent.orphan
            && let Some(Lent { done, .. }) = self.lent.take()
            && let Some(Err(err)) = done
        {
            // Nobody waits for this write: its error is the write-behind's.
            self.failed = Some(Failure::of(&err));
        }
    }

    /// A write of all of a task's own buffer (`held`, which holds nothing
    /// while it is lent): lent to the side, and written out by the driver at
    /// once if the socket may take it and on its edges if not. Ready once all
    /// of it went out (or failed), with the buffer, or what hands it back,
    /// in `held`.
    pub(super) fn write_owned(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        held: &mut Held,
        lending: &Lending<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(buf) = held.buf.as_ref() {
            if let Some(failure) = self.failed {
                return Poll::Ready(Err(failure.error()));
            }
            if let Some(err) = self.error.take() {
                return Poll::Ready(Err(err));
            }
            if buf.is_empty() && lending.to.is_none() && lending.file.is_none() {
                return Poll::Ready(Ok(0));
            }
            if self.lent.is_some() || (lending.to.is_some() && self.queued.is_some()) {
                // Another buffer is lent: this one goes after it.
                self.waiters.register(waker);
                return Poll::Pending;
            }
            self.lent = held.buf.take().map(|buf| Lent {
                buf,
                sent: 0,
                done: None,
                waker: None,
                out: false,
                orphan: false,
                to: lending.to.map(Dest::to_owned),
                #[cfg(windows)]
                file: lending.file.clone(),
                #[cfg(ring)]
                zero_copy: lending.zero_copy,
                #[cfg(ring)]
                pinned: None,
            });
        }
        // The task polls, so the driver's write needs no wake for it.
        let old = self.lent.as_mut().and_then(|l| l.waker.take());
        self.push(io, kind, after);
        let Some(lent) = self.lent.as_mut() else {
            return Poll::Ready(Err(io::Error::other("the write lost its buffer")));
        };
        if lent.done.is_none() {
            lent.waker = Some(match old {
                Some(w) if w.will_wake(waker) => w,
                other => {
                    if let Some(w) = other {
                        after.forget_one(w);
                    }
                    waker.clone()
                }
            });
            return Poll::Pending;
        }
        if let Some(w) = old {
            after.forget_one(w);
        }
        let Some(mut lent) = self.lent.take() else {
            return Poll::Ready(Err(io::Error::other("the write lost its buffer")));
        };
        let done = lent.done.take();
        *held = lent.into_held();
        // Writes that waited for the lent buffer go on.
        after.wake(&mut self.waiters);
        Poll::Ready(done.unwrap_or(Ok(0)))
    }

    /// The task of the lent buffer went away: a write none of which went
    /// out is withdrawn, one that started is left to the driver to finish,
    /// and the error of one that ended is kept as the write-behind's.
    pub(in crate::io::source) fn abandon_lent(&mut self, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        if let Some(w) = lent.waker.take() {
            after.forget_one(w);
        }
        // Out with a ring operation, some of it may be going out: its
        // completion withdraws it if none did.
        if lent.done.is_none() && (lent.sent > 0 || lent.out) {
            lent.orphan = true;
            return;
        }
        if let Some(Lent {
            done: Some(Err(err)),
            ..
        }) = self.lent.take()
        {
            self.failed = Some(Failure::of(&err));
        }
        after.wake(&mut self.waiters);
    }

    /// The lent buffer back, as it is, for a poll that ends in an error:
    /// nothing if it cannot come back now.
    pub(super) fn reclaim(&mut self, after: &mut After) -> Held {
        if self.lent.as_ref().is_some_and(|l| l.out) {
            // Out with a ring operation, it cannot come back now: the
            // write goes on as if its task went away.
            self.abandon_lent(after);
            return Held::default();
        }
        let Some(mut lent) = self.lent.take() else {
            return Held::default();
        };
        if let Some(w) = lent.waker.take() {
            after.forget_one(w);
        }
        after.wake(&mut self.waiters);
        lent.into_held()
    }
}
