//! A caller's own calls on a source: the wait until the sides it names may
//! be ready, the call, and the clear of a side the call found would block,
//! with the edge count the wait saw, so an edge after the call is never
//! lost to the clear (proofs/tla/asyncfd). `AsyncFd` and the sockets'
//! `try_io` and `async_io` all go through here.

use core::future::poll_fn;
use core::task::{Context, Poll};
use std::io;
use std::sync::Arc;

use super::{Interest, Ready, Source};

/// What a wait found: the readiness, and the edge count of each side it is
/// of.
#[derive(Clone, Copy)]
pub(crate) struct Marks {
    pub(crate) ready: Ready,
    pub(crate) read: Option<u32>,
    pub(crate) write: Option<u32>,
}

/// What of the sides `read` and `write` name may be ready now, waiting
/// until some is.
pub(crate) fn poll_marks(
    source: &Arc<Source>,
    cx: &mut Context<'_>,
    read: bool,
    write: bool,
) -> Poll<io::Result<Marks>> {
    let mut marks = Marks {
        ready: Ready::EMPTY,
        read: None,
        write: None,
    };
    if read && let Poll::Ready(tick) = source.poll_fd_read_ready(cx) {
        marks.read = Some(tick?);
        marks.ready = marks.ready | Ready::READABLE;
    }
    if write && let Poll::Ready(tick) = source.poll_fd_write_ready(cx) {
        marks.write = Some(tick?);
        marks.ready = marks.ready | Ready::WRITABLE;
    }
    if marks.ready.is_empty() {
        Poll::Pending
    } else {
        Poll::Ready(Ok(marks))
    }
}

/// The sides `marks` is of would block: each waits for its next edge,
/// unless one came after the look.
pub(crate) fn clear(source: &Arc<Source>, marks: Marks) {
    if let Some(tick) = marks.read {
        source.fd_clear_read(tick);
    }
    if let Some(tick) = marks.write {
        source.fd_clear_write(tick);
    }
}

/// The sides `interest` names.
pub(crate) fn sides(interest: Interest) -> (bool, bool) {
    (interest.is_readable(), interest.is_writable())
}

/// Runs `f` once, now, if some of the sides `read` and `write` name may be
/// ready, clearing them if `f` answers `WouldBlock`.
pub(crate) fn try_io<R>(
    source: &Arc<Source>,
    (read, write): (bool, bool),
    f: impl FnOnce() -> io::Result<R>,
) -> io::Result<R> {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let marks = match poll_marks(source, &mut cx, read, write) {
        Poll::Ready(marks) => marks?,
        Poll::Pending => return Err(io::ErrorKind::WouldBlock.into()),
    };
    let result = f();
    if matches!(&result, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
        clear(source, marks);
    }
    result
}

/// Runs `f` each time some of the sides `read` and `write` name may be
/// ready, until it answers other than `WouldBlock`, clearing the readiness
/// each time it does.
pub(crate) async fn async_io<R>(
    source: &Arc<Source>,
    (read, write): (bool, bool),
    mut f: impl FnMut() -> io::Result<R>,
) -> io::Result<R> {
    loop {
        let marks = poll_fn(|cx| poll_marks(source, cx, read, write)).await?;
        match f() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => clear(source, marks),
            result => return result,
        }
    }
}
