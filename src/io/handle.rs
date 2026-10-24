//! [`Handle`]: an IO object of std (a socket, a pipe, a file) used
//! asynchronously.
//!
//! A handle owns its IO object and a source, the part its shard's driver
//! reaches (see `io::source`). The object stays std's own type: `get_ref`
//! and `Deref` hand it out for its own methods (socket options, addresses),
//! and the handle's async methods and trait impls do the IO through the
//! source.
//!
//! # The descriptor stays the handle's
//!
//! The driver only ever makes calls on a descriptor the handle owns. Before
//! the object can change or close (`get_mut`, `DerefMut`, `into_inner`,
//! `into_raw_fd`, drop) the source is retired: its pending work stops and
//! its registration goes, so a later event for it, or a reuse of the number
//! by another file, reaches nothing. The next operation registers the
//! object again, whatever it is by then.
//!
//! The driver keeps the descriptor's number, not a borrow of it, so the
//! number must stay the object's while the handle holds it. That holds
//! because IO exists only on std's own types (the sockets, `File`), whose
//! descriptor no method on a shared borrow can close or swap: the only ways
//! to change it go through `&mut` or ownership, which retire the source
//! first. A handle over any other `AsFd` type does no IO, so its number is
//! never used past `new`. A new IO type must keep this: a type whose
//! descriptor can change behind `&self` must not get IO methods.
//!
//! # Bytes written behind
//!
//! A write is accepted into the handle's write-behind, as the kernel
//! accepts bytes into a socket's buffer, and the driver writes it out. Those
//! bytes go out whatever happens to the handle, as std's do: `get_mut` and
//! `into_inner` wait until the kernel took them, and a handle dropped before
//! that leaves them to a copy of its descriptor, which delivers them (for
//! at most [`LINGER`]) and then closes.

/// How long a dropped handle's unsent bytes may take to reach the kernel.
// vertexia: one constant for every handle; a builder setting if a peer that
// never reads must be given up on sooner or later.
pub(crate) const LINGER: Duration = Duration::from_secs(60);

use core::fmt;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::source::{Kind, Source};
use super::sys;
use crate::fs::file::FileIo;

/// An IO object (a socket, a pipe, a file) whose operations run on nuclei's
/// shards.
///
/// `Send` and `Sync` exactly when the object is.
///
/// ```
/// use std::net::TcpListener;
/// use nuclei::Handle;
///
/// let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
/// assert!(listener.local_addr().unwrap().port() > 0);
/// ```
pub struct Handle<T> {
    io: ManuallyDrop<T>,
    pub(crate) source: Arc<Source>,
    /// A regular file's IO state, made on its first operation.
    pub(crate) file: OnceLock<Arc<FileIo>>,
}

impl<T: AsFd> Handle<T> {
    /// Takes `io` over: it is made non-blocking, and what kind of
    /// descriptor it is (a socket, a file, a pipe) decides how its IO runs.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be inspected or set non-blocking.
    pub fn new(io: T) -> io::Result<Handle<T>> {
        let kind = sys::kind_of(io.as_fd().as_raw_fd())?;
        Self::adopt(io, kind)
    }

    /// Takes over `io`, a descriptor of `kind` in blocking mode or not.
    pub(crate) fn adopt(io: T, kind: Kind) -> io::Result<Handle<T>> {
        let fd = io.as_fd().as_raw_fd();
        sys::set_nonblocking(fd)?;
        if !matches!(kind, Kind::Bytes | Kind::File) {
            sys::set_nosigpipe(fd);
        }
        Ok(Self::from_nonblocking(io, kind))
    }

    /// Takes over `io`, a non-blocking descriptor of `kind` set up already
    /// (an accepted connection, a socket nuclei made).
    pub(crate) fn from_nonblocking(io: T, kind: Kind) -> Handle<T> {
        let source = Source::new(io.as_fd().as_raw_fd(), kind);
        Handle {
            io: ManuallyDrop::new(io),
            source,
            file: OnceLock::new(),
        }
    }

    /// A regular file's IO state, made now on its first use.
    pub(crate) fn file(&self) -> io::Result<&Arc<FileIo>> {
        if let Some(file) = self.file.get() {
            return Ok(file);
        }
        let file = FileIo::new(self.io.as_fd().as_raw_fd())?;
        Ok(self.file.get_or_init(|| file))
    }

    /// The source, with the object's descriptor in place: after a
    /// `get_mut` it may be another one.
    pub(crate) fn source(&self) -> &Arc<Source> {
        if self.source.is_retired() {
            self.source.revive(self.io.as_fd().as_raw_fd());
        }
        &self.source
    }
}

impl<T> Handle<T> {
    /// The IO object.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    /// The IO object, to change. Bytes written behind reach the kernel
    /// first (this waits for them), then the handle's IO stops; its next
    /// operation takes the object as it is then (another descriptor
    /// discards what the old one left).
    pub fn get_mut(&mut self) -> &mut T {
        self.deliver_unsent();
        self.hand_file_back();
        self.source.retire();
        &mut self.io
    }

    /// The IO object, back from the handle, once the bytes written behind
    /// reached the kernel (this waits for them). Bytes the handle read ahead
    /// and had not handed out are dropped with it.
    pub fn into_inner(mut self) -> T {
        self.deliver_unsent();
        self.hand_file_back();
        let mut this = ManuallyDrop::new(self);
        this.source.retire();
        // SAFETY: `this` is never used or dropped again; each field is
        // moved out once.
        unsafe {
            drop(core::ptr::read(&this.source));
            drop(core::ptr::read(&this.file));
            ManuallyDrop::take(&mut this.io)
        }
    }

    /// A regular file goes back to std where the handle's cursor is: its own
    /// offset moves there, and the handle's next operation starts from the
    /// offset std leaves.
    fn hand_file_back(&mut self) {
        if let Some(file) = self.file.take()
            && let Ok(fd) = self.source.raw_fd()
        {
            let _ = sys::seek(fd, Some(file.cursor()));
        }
    }
}

impl<T> Handle<T> {
    /// Waits until the kernel took what the write-behind holds (or
    /// [`LINGER`] passed): on a shard, its other tasks run meanwhile.
    fn deliver_unsent(&self) {
        if self.source.has_unsent() {
            let source = &self.source;
            let _ = crate::sync::internal::blocking(crate::time::timeout(
                LINGER,
                core::future::poll_fn(|cx| source.poll_drain(cx)),
            ));
        }
        if let Some(file) = self.file.get().filter(|f| f.has_unsent()) {
            let _ = crate::sync::internal::blocking(crate::time::timeout(
                LINGER,
                core::future::poll_fn(|cx| file.poll_flush(cx)),
            ));
        }
    }
}

impl<T> Drop for Handle<T> {
    /// Never waits: the source closes the object once no call of the driver
    /// can use its descriptor (at once, or as the driver lets go of it).
    fn drop(&mut self) {
        // A file's writes in flight keep their own copy of the descriptor;
        // what waits behind them is handed to a task.
        if let Some(file) = self.file.take().filter(|f| f.has_unsent()) {
            crate::runtime::spawn(async move {
                let _ =
                    crate::time::timeout(LINGER, core::future::poll_fn(|cx| file.poll_flush(cx)))
                        .await;
            })
            .detach();
        }
        // SAFETY: taken once, here; the field is never used again.
        let io = unsafe { ManuallyDrop::take(&mut self.io) };
        match self
            .source
            .has_unsent()
            .then(|| self.source.duplicate())
            .flatten()
        {
            // The unsent bytes go out through a copy of the descriptor,
            // which a task keeps and closes once they are out.
            Some(copy) => {
                self.source.close(io, Some(copy.as_raw_fd()));
                crate::runtime::spawn(linger(Arc::clone(&self.source), copy)).detach();
            }
            // The registration goes before the descriptor closes.
            None => self.source.close(io, None),
        }
    }
}

/// Delivers what a dropped handle's write-behind holds through `copy`, then
/// closes it.
async fn linger(source: Arc<Source>, copy: OwnedFd) {
    let _ = crate::time::timeout(LINGER, core::future::poll_fn(|cx| source.poll_drain(cx))).await;
    source.close(copy, None);
}

impl<T> Deref for Handle<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.io
    }
}

impl<T> DerefMut for Handle<T> {
    /// As [`Handle::get_mut`].
    fn deref_mut(&mut self) -> &mut T {
        self.get_mut()
    }
}

impl<T: fmt::Debug> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").field("io", &*self.io).finish()
    }
}

impl<T: AsFd> AsFd for Handle<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.as_fd()
    }
}

impl<T: AsRawFd> AsRawFd for Handle<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.io.as_raw_fd()
    }
}

impl<T: IntoRawFd> IntoRawFd for Handle<T> {
    fn into_raw_fd(self) -> RawFd {
        self.into_inner().into_raw_fd()
    }
}
