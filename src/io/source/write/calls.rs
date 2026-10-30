//! The source's write calls: what a handle's polls and futures call on the
//! write side.

use super::*;

impl Source {
    /// A caller's descriptor: `Ready` with the write side's edge count while
    /// it may be ready to write.
    #[cfg(unix)]
    pub(crate) fn poll_fd_write_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<u32>> {
        let kind = self.kind;
        self.poll_write_side(cx, false, |st, io, waker, after| {
            st.fd_ready(io, kind, waker, after)
        })
    }

    /// A caller's descriptor would block on a write: the side waits for its
    /// next edge, unless one came after `tick`.
    #[cfg(unix)]
    pub(crate) fn fd_clear_write(self: &Arc<Self>, tick: u32) {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        // Nothing to wait for and nothing to fail: a registration that
        // failed is the next readiness call's to report.
        let _ = self.poll_write_side(&mut cx, false, |st, _io, _waker, _after| {
            st.fd_clear(tick);
            Poll::Ready(Ok(()))
        });
    }

    /// Ready once a write would be taken at once (see `WriteState::ready`);
    /// `true` once writing failed.
    pub(crate) fn poll_write_ready(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<bool>> {
        self.poll_writing(cx, |st, _io, waker, _after| st.ready(waker).map(Ok))
    }

    /// Writes `bytes` to a stream: copied into the write-behind and written
    /// out by the driver.
    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(bytes)])
    }

    /// Writes the bytes of `bufs`, gathered in order, as
    /// [`poll_write`](Self::poll_write) writes one slice.
    pub(crate) fn poll_write_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        crate::coop::poll_budget(cx, |cx| {
            self.poll_writing(cx, |st, io, waker, after| {
                #[cfg(unix)]
                if super::super::direct::worth(io, bufs.iter().map(|b| b.len()).sum(), true)
                    && let Some(result) = st.write_direct(io, self.serving_fd(None), kind, bufs)
                {
                    return Poll::Ready(result);
                }
                st.write(io, kind, bufs, waker, after)
            })
        })
    }

    /// Polls the write side with `step` for the handle's own writes, given
    /// the way this thread reaches the descriptor.
    fn poll_writing<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        step: impl FnMut(&mut WriteState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        self.poll_write_side(cx, true, step)
    }

    /// Polls the write side with `step`: for the handle's own writes when
    /// `drive`, else for its caller's calls.
    fn poll_write_side<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        drive: bool,
        mut step: impl FnMut(&mut WriteState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(|io| {
            self.write.poll(
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

    /// Writes all of `buf` and gives it back with the write's result. The
    /// buffer is lent to the side until the kernel took the last byte, so
    /// the bytes go out from it with no copy.
    pub(crate) async fn write_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
    ) -> (io::Result<usize>, Vec<u8>) {
        let mut op = WriteOwned {
            source: self,
            buf: Some(buf),
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.buf.take().unwrap_or_default())
    }

    fn poll_write_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<Vec<u8>>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        let poll = self.poll_writing(cx, |st, io, waker, after| {
            st.write_owned(io, kind, held, waker, after)
        });
        if poll.is_ready() && held.is_none() {
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

    /// Completes once the write-behind is out (or reports why it stopped).
    pub(crate) fn poll_flush(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| st.flush(io, kind, waker, after))
    }

    /// Flushes, then shuts a stream socket's writing half down.
    pub(crate) fn poll_close(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.close(io, fd, kind, waker, after)
        })
    }

    /// Sends a datagram to `dest`: copied, queued and sent by the driver.
    pub(crate) fn poll_send(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
        dest: Dest<'_>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.send(io, kind, bytes, dest, waker, after)
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
struct WriteOwned<'a> {
    source: &'a Arc<Source>,
    buf: Option<Vec<u8>>,
    lent: bool,
}

impl WriteOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self.source.poll_write_owned(cx, &mut self.buf);
        self.lent = self.buf.is_none();
        poll
    }
}

impl Drop for WriteOwned<'_> {
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
