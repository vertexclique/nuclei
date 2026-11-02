//! The source's read calls: what a handle's polls and futures call on the
//! read side.

use super::*;
use crate::buf::owned::{IoBufMut, OwnedBuf};
use crate::io::side::OwnerPoll;

impl Source {
    /// A caller's descriptor: `Ready` with the read side's edge count while
    /// it may be ready to read.
    #[cfg(unix)]
    pub(crate) fn poll_fd_read_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<u32>> {
        self.poll_read_side(
            || cx.waker(),
            false,
            |st, io, waker, after| st.fd_ready(io, waker, after),
        )
    }

    /// A caller's descriptor would block on a read: the side waits for its
    /// next edge, unless one came after `tick`. A side held by another
    /// (another task's look, the driver's edge) has its holder clear it as
    /// it lets go, before any look of the caller's can see it ready.
    #[cfg(unix)]
    pub(crate) fn fd_clear_read(self: &Arc<Self>, tick: u32) {
        let mut after = After::default();
        // Nothing to fail: a registration that failed is the next readiness call's to report.
        let _ = self.with_io(|io| {
            self.read.run_or_defer(
                super::super::read_work::CLEAR,
                &mut after,
                &self.hooks_in(io.shard),
                |st| st.fd_clear(tick),
                || self.read_clear.leave(tick),
            );
        });
        self.settle(after);
    }

    /// Drops what the side read ahead of a stream and holds still
    /// (`ReadState::drop_ahead`), once the kernel shut the reading half.
    pub(crate) fn poll_drop_ahead(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_reading(cx, |st, _io, _waker, after| {
            st.drop_ahead(after);
            Poll::Ready(Ok(()))
        })
    }

    /// Ready once a read would end at once (see `ReadState::ready`); `true`
    /// at the end of the stream.
    pub(crate) fn poll_read_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<bool>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.ready(
                io,
                self.serving_fd(None),
                kind,
                self.datagrams_ahead(),
                waker,
                after,
            )
            .map(Ok)
        })
    }

    /// Reads from a stream into `out`; `peek` leaves the bytes to be read
    /// again.
    pub(crate) fn poll_read(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        crate::coop::poll_budget(
            cx,
            #[inline(always)]
            |cx| {
                let cx_waker = cx.waker();
                if let Some(io) = self.here(&self.read) {
                    owner_poll!(self, self.read, io, cx_waker, |st, after, waker| self
                        .read_step(st, io, waker, after, out, peek));
                }
                self.poll_read_any(cx_waker, out, peek)
            },
        )
    }

    /// [`poll_read`](Self::poll_read) for every poll [`here`](Self::here) does not take, given
    /// the poll's waker, all of its context a read looks at.
    #[inline(never)]
    fn poll_read_any(
        self: &Arc<Self>,
        waker: &Waker,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<usize>> {
        self.poll_read_side(
            || waker,
            true,
            #[inline(always)]
            |st, io, waker, after| self.read_step(st, io, waker, after, out, peek),
        )
    }

    /// A stream read's step: straight into `out` when it goes there (`read_direct`), else
    /// through the read-ahead.
    #[inline(always)]
    fn read_step(
        &self,
        st: &mut ReadState,
        io: Io<'_>,
        waker: &Waker,
        after: &mut After,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        #[cfg(unix)]
        if let Some(poll) = st.read_direct(
            io,
            self.serving_fd(None),
            kind,
            out.len(),
            peek,
            waker,
            #[inline(always)]
            |fd, inq| {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                if inq {
                    return sys::read_inq(fd, out);
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                let _ = inq;
                let read = if peek {
                    sys::peek(fd, out)
                } else {
                    sys::read(fd, out)
                };
                read.map(|n| (n, None))
            },
        ) {
            return poll;
        }
        st.read(io, kind, out, peek, waker, after)
    }

    /// Polls the read side with `step` for the handle's own reads, given
    /// the way this thread reaches the descriptor.
    #[inline(always)]
    fn poll_reading<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        step: impl FnMut(&mut ReadState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        self.poll_read_side(|| cx.waker(), true, step)
    }

    /// Polls the read side with `step`: for the handle's own reads when
    /// `drive`, else for its caller's calls. `waker` gives the poll's waker
    /// where it is used, so a caller that has its context reads it there.
    #[inline(always)]
    fn poll_read_side<'w, R>(
        self: &Arc<Self>,
        waker: impl Fn() -> &'w Waker,
        drive: bool,
        mut step: impl FnMut(&mut ReadState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(
            #[inline(always)]
            |io| {
                self.read.poll(
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

    /// Reads into the spare capacity of `buf`, after the bytes it holds,
    /// and gives it back with the read's result. The buffer is lent to the
    /// side until the driver read into it, so the bytes land in it with no
    /// copy (bytes another read left behind are copied in instead).
    pub(crate) async fn read_owned<B>(self: &Arc<Self>, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBufMut,
    {
        let mut op = ReadOwned {
            source: self,
            buf: Some(OwnedBuf::filled(buf)),
            from: None,
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.buf.take().unwrap_or_default().into_filled())
    }

    /// Receives one datagram into the spare capacity of `buf`, after the
    /// bytes it holds, and gives it back with the datagram's length and
    /// sender. The buffer is lent to the side until the driver received into
    /// it, so the datagram lands in it with no copy (one that came before is
    /// copied in instead); what does not fit is cut.
    pub(crate) async fn recv_from_owned<B>(
        self: &Arc<Self>,
        buf: B,
    ) -> (io::Result<(usize, Addr)>, B)
    where
        B: IoBufMut,
    {
        let (result, buf) = self.recv_from_buf(OwnedBuf::filled(buf)).await;
        (result, buf.into_filled())
    }

    async fn recv_from_buf(
        self: &Arc<Self>,
        mut buf: OwnedBuf,
    ) -> (io::Result<(usize, Addr)>, OwnedBuf) {
        let start = buf.len();
        let room = buf.capacity() - start;
        if room < super::super::OWNED_DATAGRAM_MIN && self.on_ring() {
            // A small datagram is cheaper taken as every other is and copied in than received
            // by an operation of its own.
            let spare = buf.spare_zeroed();
            let received = poll_fn(|cx| self.poll_receive(cx, &mut *spare, false)).await;
            return match received {
                Ok((n, from)) => {
                    // SAFETY: the spare capacity was zeroed and the datagram's
                    // `n` bytes received into its start.
                    unsafe { buf.set_len(start + n.min(room)) };
                    (Ok((n, from)), buf)
                }
                Err(err) => (Err(err), buf),
            };
        }
        let mut op = ReadOwned {
            source: self,
            buf: Some(buf),
            from: None,
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        let result = result.and_then(|n| {
            op.from
                .take()
                .map(|from| (n, from))
                .ok_or_else(|| io::Error::other("the datagram came with no sender"))
        });
        (result, op.buf.take().unwrap_or_default())
    }

    fn poll_read_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<OwnedBuf>,
        from: &mut Option<Addr>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        if !kind.datagrams() && held.as_ref().is_some_and(|b| b.len() == b.capacity()) {
            return Poll::Ready(Ok(0));
        }
        let poll = self.poll_reading(cx, |st, io, waker, after| {
            if kind.datagrams() {
                st.read_owned_from(io, kind, held, from, waker, after)
            } else {
                st.read_owned(io, kind, held, waker, after)
            }
        });
        if poll.is_ready() && held.is_none() {
            // The poll failed with the buffer lent: it comes back as it is.
            // If the side is held now, the poll waits and starts over.
            let mut after = After::default();
            let back = self
                .read
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

    /// Reads from a stream, scattered into `bufs` in order.
    pub(crate) fn poll_read_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        if bufs.iter().all(|b| b.is_empty()) {
            return Poll::Ready(Ok(0));
        }
        let kind = self.kind;
        crate::coop::poll_budget(cx, |cx| {
            self.poll_reading(cx, |st, io, waker, after| {
                #[cfg(unix)]
                {
                    let len = bufs.iter().map(|b| b.len()).sum();
                    if let Some(poll) = st.read_direct(
                        io,
                        self.serving_fd(None),
                        kind,
                        len,
                        false,
                        waker,
                        |fd, _| sys::read_bufs(fd, bufs).map(|n| (n, None)),
                    ) {
                        return poll;
                    }
                }
                st.read_vectored(io, kind, bufs, waker, after)
            })
        })
    }

    /// Accepts a connection from a listener.
    pub(crate) fn poll_accept(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Owned, Addr)>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| st.accept(io, kind, waker, after))
    }

    /// Accepts a connection, without its peer's address.
    pub(crate) fn poll_accept_stream(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Owned>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.accept_stream(io, kind, waker, after)
        })
    }

    /// Receives a datagram into `out`, with its sender; `peek` leaves it to
    /// be received again.
    pub(crate) fn poll_receive(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, Addr)>> {
        self.poll_receive_as(cx, out, peek)
    }

    /// [`poll_receive_as`](Self::poll_receive_as) through the side, `seen`
    /// the word a call made with no guard saw before the kernel said "not
    /// now". One copy for every sender type and every caller, out of line: the
    /// side's path stays one, as the stream calls' paths through it are.
    #[inline(never)]
    fn poll_receive_held(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
        #[cfg_attr(
            not(unix),
            expect(unused_variables, reason = "only unix calls with no guard")
        )]
        seen: Option<u32>,
    ) -> Poll<io::Result<(usize, Addr)>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            #[cfg(unix)]
            if let Some(poll) = st.receive_now::<Addr>(
                io,
                self.serving_fd(None),
                kind,
                self.datagrams_ahead(),
                out,
                peek,
                seen,
                waker,
                after,
            ) {
                return poll;
            }
            st.receive(io, kind, out, peek, waker, after)
        })
    }

    /// [`poll_receive`](Self::poll_receive), with the sender as the handle
    /// hands it out. On unix a side that holds nothing has the task's own
    /// call made at once, with no guard; one the kernel answers "not now"
    /// goes through the side with the word it saw (see `quick`).
    #[inline(always)]
    pub(crate) fn poll_receive_as<A: Sender>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, A)>> {
        #[cfg(unix)]
        let seen = match self.quick.read_clear() {
            Some(word) => {
                let fd = match self.fd() {
                    Ok(fd) => fd,
                    Err(err) => return Poll::Ready(Err(err)),
                };
                match sys::ready(A::receive(fd, self.kind, out, peek)) {
                    Some(result) => {
                        if !peek {
                            tally::received(&result);
                        }
                        return Poll::Ready(result);
                    }
                    None => Some(word),
                }
            }
            None => None,
        };
        #[cfg(not(unix))]
        let seen = None;
        self.poll_receive_held(cx, out, peek, seen)
            .map(|r| r.and_then(|(n, addr)| A::from_addr(addr).map(|a| (n, a))))
    }
}

/// A read whose buffer may be lent to the side. Dropped while it is, the
/// side keeps what the driver read into it for the next read.
struct ReadOwned<'a> {
    source: &'a Arc<Source>,
    buf: Option<OwnedBuf>,
    /// The sender, for a datagram socket's receive.
    from: Option<Addr>,
    lent: bool,
}

impl ReadOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self
            .source
            .poll_read_owned(cx, &mut self.buf, &mut self.from);
        self.lent = self.buf.is_none();
        poll
    }
}

impl Drop for ReadOwned<'_> {
    fn drop(&mut self) {
        if self.lent {
            let mut after = After::default();
            self.source.read.defer(
                super::super::read_work::ABANDON,
                &mut after,
                &self.source.hooks(),
            );
            self.source.settle(after);
        }
    }
}
