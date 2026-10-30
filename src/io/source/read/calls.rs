//! The source's read calls: what a handle's polls and futures call on the
//! read side.

use super::*;

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
            st.ready(io, kind, waker, after).map(Ok)
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
    pub(crate) async fn read_owned(self: &Arc<Self>, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        let mut op = ReadOwned {
            source: self,
            buf: Some(buf),
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.buf.take().unwrap_or_default())
    }

    fn poll_read_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<Vec<u8>>,
    ) -> Poll<io::Result<usize>> {
        if held.as_ref().is_some_and(|b| b.len() == b.capacity()) {
            return Poll::Ready(Ok(0));
        }
        let kind = self.kind;
        let poll = self.poll_reading(cx, |st, io, waker, after| {
            st.read_owned(io, kind, held, waker, after)
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
            st.receive(io, kind, out, peek, waker, after)
        })
    }
}

/// A read whose buffer may be lent to the side. Dropped while it is, the
/// side keeps what the driver read into it for the next read.
struct ReadOwned<'a> {
    source: &'a Arc<Source>,
    buf: Option<Vec<u8>>,
    lent: bool,
}

impl ReadOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self.source.poll_read_owned(cx, &mut self.buf);
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
