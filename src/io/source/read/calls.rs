//! The source's read calls: what a handle's polls and futures call on the
//! read side.

use super::*;
use crate::buf::owned::{IoBufMut, OwnedBuf};

impl Source {
    /// A caller's descriptor: `Ready` with the read side's edge count while
    /// it may be ready to read.
    #[cfg(unix)]
    pub(crate) fn poll_fd_read_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<u32>> {
        self.poll_read_side(cx, false, |st, io, waker, after| {
            st.fd_ready(io, waker, after)
        })
    }

    /// A caller's descriptor would block on a read: the side waits for its
    /// next edge, unless one came after `tick`.
    #[cfg(unix)]
    pub(crate) fn fd_clear_read(self: &Arc<Self>, tick: u32) {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        // Nothing to wait for and nothing to fail: a registration that
        // failed is the next readiness call's to report.
        let _ = self.poll_read_side(&mut cx, false, |st, _io, _waker, _after| {
            st.fd_clear(tick);
            Poll::Ready(Ok(()))
        });
    }

    /// Ready once a read would end at once (see `ReadState::ready`); `true`
    /// at the end of the stream.
    pub(crate) fn poll_read_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<bool>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.ready(io, self.serving_fd(None), kind, waker, after)
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
        let kind = self.kind;
        crate::coop::poll_budget(cx, |cx| {
            self.poll_reading(cx, |st, io, waker, after| {
                #[cfg(unix)]
                if st.reads_itself(io, kind, out.len(), peek)
                    && let Some(poll) = st.read_direct(
                        io,
                        self.serving_fd(None),
                        kind,
                        out.len(),
                        peek,
                        waker,
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
                    )
                {
                    return poll;
                }
                st.read(io, kind, out, peek, waker, after)
            })
        })
    }

    /// Polls the read side with `step` for the handle's own reads, given
    /// the way this thread reaches the descriptor.
    fn poll_reading<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        step: impl FnMut(&mut ReadState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        self.poll_read_side(cx, true, step)
    }

    /// Polls the read side with `step`: for the handle's own reads when
    /// `drive`, else for its caller's calls.
    fn poll_read_side<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        drive: bool,
        mut step: impl FnMut(&mut ReadState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(|io| {
            self.read.poll(
                &mut after,
                cx.waker(),
                &self.hooks_in(io.shard),
                |st, after| {
                    #[cfg(unix)]
                    if drive {
                        st.drive();
                    }
                    #[cfg(not(unix))]
                    let _ = drive;
                    step(st, io, cx.waker(), after)
                },
            )
        });
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
                    if st.reads_itself(io, kind, len, false)
                        && let Some(poll) = st.read_direct(
                            io,
                            self.serving_fd(None),
                            kind,
                            len,
                            false,
                            waker,
                            |fd, _| sys::read_bufs(fd, bufs).map(|n| (n, None)),
                        )
                    {
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
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.receive(io, self.serving_fd(None), kind, out, peek, waker, after)
        })
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
