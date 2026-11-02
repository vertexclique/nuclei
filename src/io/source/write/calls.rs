//! The source's write calls: what a handle's polls and futures call on the
//! write side.

use super::*;
use crate::buf::owned::{IoBuf, OwnedBuf};
use crate::io::side::OwnerPoll;

#[cfg(all(test, unix))]
thread_local! {
    /// Runs once on this thread, for a test, between an owner's straight send that found the socket
    /// full and the rest of its poll.
    pub(crate) static AFTER_FULL_SEND: core::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { core::cell::Cell::new(None) };
}

impl Source {
    /// A caller's descriptor: `Ready` with the write side's edge count while
    /// it may be ready to write.
    #[cfg(unix)]
    pub(crate) fn poll_fd_write_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<u32>> {
        let kind = self.kind;
        self.poll_write_side(
            || cx.waker(),
            false,
            |st, io, waker, after| st.fd_ready(io, kind, waker, after),
        )
    }

    /// A caller's descriptor would block on a write: the side waits for its
    /// next edge, unless one came after `tick`; as
    /// [`fd_clear_read`](Self::fd_clear_read) for a held side.
    #[cfg(unix)]
    pub(crate) fn fd_clear_write(self: &Arc<Self>, tick: u32) {
        let mut after = After::default();
        // Nothing to fail: a registration that failed is the next readiness call's to report.
        let _ = self.with_io(|io| {
            self.write.run_or_defer(
                super::super::write_work::CLEAR,
                &mut after,
                &self.hooks_in(io.shard),
                |st| st.fd_clear(tick),
                || self.write_clear.leave(tick),
            );
        });
        self.settle(after);
    }

    /// Ready once a write would be taken at once (see `WriteState::ready`);
    /// `true` once writing failed.
    pub(crate) fn poll_write_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<bool>> {
        self.poll_writing(cx, |st, _io, waker, _after| st.ready(waker).map(Ok))
    }

    /// Writes `bytes` to a stream: straight to the kernel while the side
    /// holds nothing, else copied into the write-behind and written out by
    /// the driver. A large stream write waits instead of being copied (see
    /// `write_direct`).
    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(bytes)])
    }

    /// Writes the bytes of `bufs`, gathered in order, as
    /// [`poll_write`](Self::poll_write) writes one slice.
    #[inline(always)]
    pub(crate) fn poll_write_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_bufs(cx, bufs, true)
    }

    /// A write of `bufs` that never waits for the socket: what the kernel
    /// takes now, or else a copy into the write-behind (`try_write`).
    pub(crate) fn poll_write_behind(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_bufs(cx, bufs, false)
    }

    /// The writes of `bufs`; a large stream write waits for the socket when
    /// `wait`. Inlined, so a single slice's length is known where the call
    /// is made.
    #[inline(always)]
    fn poll_write_bufs(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
        wait: bool,
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(
            cx,
            #[inline(always)]
            |cx| {
                let cx_waker = cx.waker();
                // Whether the owner's send below found the socket full, so the owner's step does
                // not ask the kernel again.
                #[cfg(unix)]
                let mut asked = false;
                #[cfg(unix)]
                if let Some(result) = self.send_as_owner(bufs, &mut asked) {
                    return Poll::Ready(result);
                }
                #[cfg(all(test, unix))]
                if asked && let Some(then) = AFTER_FULL_SEND.take() {
                    then();
                }
                #[cfg(not(unix))]
                let asked = false;
                if let Some(io) = self.here(&self.write) {
                    owner_poll!(self, self.write, io, cx_waker, |st, after, waker| self
                        .write_step(st, io, waker, after, bufs, wait, asked));
                }
                self.poll_write_any(cx_waker, bufs, wait)
            },
        )
    }

    /// A stream write by the write side's owner, on the shard the source is homed on, made
    /// straight from `bufs` under the side's biased hold with nothing else of a poll, while the
    /// side's last release said it holds nothing (the `WRITE_SENDS` bit of `unsent`,
    /// `WriteState::sends_now`, proofs/tla/sendsnow/SendsNow.tla; the hold's take catches the
    /// side up with a revival first, which only clears it): what the kernel took, or `None` for
    /// the poll's own way. The biased hold is refused once a foreign thread came, so every release
    /// since the bit's was this shard's own, and none was since. The hold changes
    /// nothing of the side, so nothing is published as it lets go: a socket found full is the
    /// owner's poll's to record (`asked`), which then does not ask the kernel again; a poll whose
    /// shard was taken since asks it itself (`poll_write_any`).
    #[cfg(unix)]
    #[inline(always)]
    fn send_as_owner(
        self: &Arc<Self>,
        bufs: &[io::IoSlice<'_>],
        asked: &mut bool,
    ) -> Option<io::Result<usize>> {
        let shard = crate::rt::context::running(crate::rt::context::local())?;
        if !self.kind.streams() {
            return None;
        }
        let addr = shard.shared_addr();
        if self.reg.home.load(core::sync::atomic::Ordering::Relaxed) != addr
            || !self.write.owned_by(addr)
        {
            return None;
        }
        let mut after = After::default();
        // Marked until the settle, as the owner's poll is (`owner_poll`).
        if !after.enter() {
            return None;
        }
        let sent = self.write.with_owner(
            &mut after,
            &self.hooks_in(Some(shard)),
            #[inline(always)]
            |_| {
                // Relaxed: only this shard ever held the side, so the last release that stored
                // the bit was this thread's, or came before the handoff's fence.
                if self.unsent.load(core::sync::atomic::Ordering::Relaxed)
                    & super::super::WRITE_SENDS
                    == 0
                    || bufs.iter().all(|b| b.is_empty())
                {
                    return None;
                }
                let fd = self.serving_fd(None)?;
                Some(sys::ready(sys::send_bufs(fd, bufs)))
            },
        );
        self.settle_owned(after);
        match sent?? {
            Some(Ok(0)) => Some(Err(io::ErrorKind::WriteZero.into())),
            Some(result) => {
                tally::writing(&result);
                Some(result)
            }
            None => {
                *asked = true;
                None
            }
        }
    }

    /// [`poll_write_bufs`](Self::poll_write_bufs) for every poll [`here`](Self::here) does not
    /// take, given the poll's waker, all of its context a write looks at. Its step asks the kernel
    /// itself, whatever the owner's send found: a poll comes here after that send when its shard
    /// was taken in between, and the shard's driver on its new thread may have passed over the
    /// edge the refusal owed, the side still saying it was only ever its owner's and idle
    /// (proofs/tla/sendsnow/TakenFull.tla).
    #[inline(never)]
    fn poll_write_any(
        self: &Arc<Self>,
        waker: &Waker,
        bufs: &[io::IoSlice<'_>],
        wait: bool,
    ) -> Poll<io::Result<usize>> {
        self.poll_write_side(
            || waker,
            true,
            #[inline(always)]
            |st, io, waker, after| self.write_step(st, io, waker, after, bufs, wait, false),
        )
    }

    /// A stream write's step: straight from `bufs` while the side holds nothing
    /// (`write_direct`), unless the kernel just took nothing (`asked`), else through the
    /// write-behind.
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    #[inline(always)]
    fn write_step(
        &self,
        st: &mut WriteState,
        io: Io<'_>,
        waker: &Waker,
        after: &mut After,
        bufs: &[io::IoSlice<'_>],
        wait: bool,
        asked: bool,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        #[cfg(unix)]
        if asked {
            st.found_full(matches!(io.via, Via::Fd(_)));
        } else if let Some(result) = st.write_direct(io, self.serving_fd(None), kind, bufs) {
            return Poll::Ready(result);
        }
        #[cfg(not(unix))]
        let _ = asked;
        st.write(io, kind, bufs, wait, waker, after)
    }

    /// Polls the write side with `step` for the handle's own writes, given
    /// the way this thread reaches the descriptor.
    #[inline(always)]
    pub(super) fn poll_writing<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        step: impl FnMut(&mut WriteState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        self.poll_write_side(|| cx.waker(), true, step)
    }

    /// Polls the write side with `step`: for the handle's own writes when
    /// `drive`, else for its caller's calls. `waker` gives the poll's waker
    /// where it is used, as the read side's does.
    #[inline(always)]
    fn poll_write_side<'w, R>(
        self: &Arc<Self>,
        waker: impl Fn() -> &'w Waker,
        drive: bool,
        mut step: impl FnMut(&mut WriteState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(
            #[inline(always)]
            |io| {
                self.write.poll(
                    &mut after,
                    waker(),
                    &self.hooks_in(io.shard),
                    #[inline(always)]
                    |st, after| {
                        #[cfg(unix)]
                        if drive {
                            st.drive();
                        }
                        #[cfg(not(unix))]
                        let _ = drive;
                        step(st, io, waker(), after)
                    },
                )
            },
        );
        match polled {
            Ok(poll) => self.finish_poll(after, poll),
            Err(err) => {
                self.settle(after);
                Poll::Ready(Err(err))
            }
        }
    }

    /// Writes all of `buf` and gives it back with the write's result. The
    /// buffer is lent to the side until the kernel took the last byte, so
    /// the bytes go out from it with no copy.
    pub(crate) async fn write_owned<B>(self: &Arc<Self>, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        let (result, mut held) = self
            .lend_write(OwnedBuf::written(buf), Lending::default())
            .await;
        (result, held.buf.take().unwrap_or_default().into_written())
    }

    /// Writes all of `buf` as [`write_owned`](Self::write_owned) does, the ring sending its large
    /// chunks zero-copy (see `RingIssuer::zero_copy`): the write ends once the kernel took the
    /// last byte, and the buffer comes back through the [`Released`](crate::net::Released) once
    /// the kernel let go of every byte. A write the kernel copies has its buffer back at once.
    pub(crate) async fn write_zero_copy<B>(
        self: &Arc<Self>,
        buf: B,
    ) -> (io::Result<usize>, crate::net::Released<B>)
    where
        B: IoBuf,
    {
        let buf = OwnedBuf::written(buf);
        #[cfg(ring)]
        if self.kind == Kind::Tcp {
            let lending = Lending {
                zero_copy: true,
                ..Lending::default()
            };
            let (result, mut held) = self.lend_write(buf, lending).await;
            return (result, held.released());
        }
        let (result, mut held) = self.lend_write(buf, Lending::default()).await;
        (
            result,
            crate::net::Released::ready(held.buf.take().unwrap_or_default()),
        )
    }

    /// Sends all of `buf` as one datagram to `to` and gives it back with the
    /// send's result. The buffer is lent to the side until the kernel took
    /// the datagram, so the bytes go out from it with no copy.
    pub(crate) async fn send_owned<B>(
        self: &Arc<Self>,
        buf: B,
        to: Dest<'_>,
    ) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        let buf = OwnedBuf::written(buf);
        if buf.len() < super::super::OWNED_DATAGRAM_MIN && self.on_ring() {
            // A small datagram is cheaper sent by the call itself, which ends at once, than
            // lent to the ring and waited for.
            let result = poll_fn(|cx| self.poll_send(cx, &buf, to)).await;
            return (result, buf.into_written());
        }
        let lending = Lending {
            to: Some(to),
            ..Lending::default()
        };
        let (result, mut held) = self.lend_write(buf, lending).await;
        (result, held.buf.take().unwrap_or_default().into_written())
    }

    /// Lends `buf` to the side for a write as `lending` says, and gives back
    /// the write's result with what the task holds of the buffer then.
    async fn lend_write(
        self: &Arc<Self>,
        buf: OwnedBuf,
        lending: Lending<'_>,
    ) -> (io::Result<usize>, Held) {
        let mut op = WriteOwned {
            source: self,
            held: Held::of(buf),
            lending,
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, core::mem::take(&mut op.held))
    }

    fn poll_write_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Held,
        lending: &Lending<'_>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        let poll = self.poll_writing(cx, |st, io, waker, after| {
            st.write_owned(io, kind, held, lending, waker, after)
        });
        if poll.is_ready() && held.lent() {
            // The poll failed with the buffer lent: it comes back as it is.
            // If the side is held now, the poll waits and starts over.
            let mut after = After::default();
            let back = self
                .write
                .poll(&mut after, cx.waker(), &self.hooks(), |st, after| {
                    Poll::Ready(st.reclaim(after))
                });
            self.settle(after);
            match back {
                Poll::Ready(buf) => *held = buf,
                Poll::Pending => return Poll::Pending,
            }
        }
        poll
    }

    /// Sends `len` bytes of the file `handle` names, from `offset`, to a stream in the kernel, and
    /// answers how many went. The handle is kept open until the send ends, and a task that goes
    /// away leaves the send to the driver, which finishes what started.
    #[cfg(windows)]
    pub(crate) async fn send_file_range(
        self: &Arc<Self>,
        handle: Arc<crate::io::raw::OwnedFile>,
        offset: u64,
        len: u32,
    ) -> io::Result<usize> {
        let mut op = WriteOwned {
            source: self,
            held: Held::of(OwnedBuf::default()),
            lending: Lending {
                file: Some(FileRange {
                    handle,
                    offset,
                    len,
                }),
                ..Lending::default()
            },
            lent: false,
        };
        poll_fn(|cx| op.poll(cx)).await
    }

    /// Completes once the write-behind is out (or reports why it stopped).
    pub(crate) fn poll_flush(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| st.flush(io, kind, waker, after))
    }

    /// Flushes, then shuts a stream socket's writing half down.
    pub(crate) fn poll_close(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_shutdown(cx, std::net::Shutdown::Write)
    }

    /// Flushes, then shuts the halves `how` names of a stream socket down:
    /// the writing half, or both.
    pub(crate) fn poll_shutdown(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        how: std::net::Shutdown,
    ) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.close(io, fd, kind, how, waker, after)
        })
    }

    /// Sends a datagram to `dest`: on unix handed to the kernel by the task
    /// itself, at once and with no guard while the side holds nothing (see
    /// `quick`); on a port copied, queued and sent by the driver.
    #[inline(always)]
    pub(crate) fn poll_send(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
        dest: Dest<'_>,
    ) -> Poll<io::Result<usize>> {
        #[cfg(unix)]
        if self.quick.write_clear() {
            let fd = match self.fd() {
                Ok(fd) => fd,
                Err(err) => return Poll::Ready(Err(err)),
            };
            if let Some(result) = sys::ready(send_datagram(fd, bytes, dest)) {
                tally::writing(&result);
                return Poll::Ready(result);
            }
        }
        self.poll_send_held(cx, bytes, dest)
    }

    /// [`poll_send`](Self::poll_send) through the side. One copy for every
    /// caller, out of line: the side's path stays one, as the stream calls'
    /// paths through it are.
    #[inline(never)]
    fn poll_send_held(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
        dest: Dest<'_>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            // Raised before the side's own call: every writable edge after a call that finds
            // the socket full reaches the side.
            #[cfg(unix)]
            self.quick.hold_write();
            st.send(io, self.serving_fd(None), kind, bytes, dest, waker, after)
        })
    }

    /// Completes once the kernel took every byte and datagram accepted (or
    /// a failure ended them).
    pub(crate) fn poll_drain(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<()> {
        let kind = self.kind;
        match self.poll_writing(cx, |st, io, waker, after| {
            st.drain(io, kind, waker, after).map(Ok)
        }) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Connects the socket to `peer`: the driver starts the connect on the
    /// first poll and sees it end. Polled again after it completes, it
    /// completes at once.
    pub(crate) fn poll_connect(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        peer: Peer<'_>,
    ) -> Poll<io::Result<()>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.connect(io, kind, peer, waker, after)
        })
    }
}

/// A write whose buffer may be lent to the side. Dropped while it is, the
/// side withdraws the write if none of it went out and finishes it if some
/// did.
struct WriteOwned<'a, 'd> {
    source: &'a Arc<Source>,
    held: Held,
    /// Where a datagram goes; `None` for a stream's bytes.
    lending: Lending<'d>,
    lent: bool,
}

impl WriteOwned<'_, '_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self
            .source
            .poll_write_owned(cx, &mut self.held, &self.lending);
        self.lent = self.held.lent();
        poll
    }
}

impl Drop for WriteOwned<'_, '_> {
    fn drop(&mut self) {
        if self.lent {
            let mut after = After::default();
            self.source.write.defer(
                super::super::write_work::ABANDON,
                &mut after,
                &self.source.hooks(),
            );
            self.source.settle(after);
        }
    }
}
