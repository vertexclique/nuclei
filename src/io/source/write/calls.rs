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
            held: Held::of(buf),
            lending: Lending::default(),
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.held.buf.take().unwrap_or_default())
    }

    /// Writes all of `buf` as [`write_owned`](Self::write_owned) does, the ring sending its large
    /// chunks zero-copy (see `ring::zero_copy_worth`): the write ends once the kernel took the
    /// last byte, and the buffer comes back through the [`Released`](crate::net::Released) once
    /// the kernel let go of every byte. A write the kernel copies has its buffer back at once.
    pub(crate) async fn write_zero_copy(
        self: &Arc<Self>,
        buf: Vec<u8>,
    ) -> (io::Result<usize>, crate::net::Released) {
        #[cfg(ring)]
        if super::super::ring::zero_copy_worth(self.kind, buf.len()) {
            let mut op = WriteOwned {
                source: self,
                held: Held::of(buf),
                lending: Lending {
                    zero_copy: true,
                    ..Lending::default()
                },
                lent: false,
            };
            let result = poll_fn(|cx| op.poll(cx)).await;
            return (result, op.held.released());
        }
        let (result, buf) = self.write_owned(buf).await;
        (result, crate::net::Released::ready(buf))
    }

    /// Sends all of `buf` as one datagram to `to` and gives it back with the
    /// send's result. The buffer is lent to the side until the kernel took
    /// the datagram, so the bytes go out from it with no copy.
    pub(crate) async fn send_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
        to: Dest<'_>,
    ) -> (io::Result<usize>, Vec<u8>) {
        if buf.len() < super::super::OWNED_DATAGRAM_MIN && self.on_ring() {
            // A small datagram is cheaper copied and queued, which ends at once, than lent
            // to the ring and waited for.
            let result = poll_fn(|cx| self.poll_send(cx, &buf, to)).await;
            return (result, buf);
        }
        let mut op = WriteOwned {
            source: self,
            held: Held::of(buf),
            lending: Lending {
                to: Some(to),
                ..Lending::default()
            },
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.held.buf.take().unwrap_or_default())
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
            held: Held::of(Vec::new()),
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
