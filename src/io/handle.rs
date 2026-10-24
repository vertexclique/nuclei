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

use core::fmt;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, RawFd};
use std::sync::Arc;

use super::source::{Kind, Source};
use super::sys;

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
        }
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

    /// The IO object, to change. Its pending IO stops first; the handle's
    /// next operation takes the object as it is then (another descriptor
    /// discards what the old one left).
    pub fn get_mut(&mut self) -> &mut T {
        self.source.retire();
        &mut self.io
    }

    /// The IO object, back from the handle. Bytes the handle read ahead and
    /// had not handed out are dropped with it; written bytes it still held
    /// are not written.
    pub fn into_inner(self) -> T {
        let mut this = ManuallyDrop::new(self);
        this.source.retire();
        // SAFETY: `this` is never used or dropped again; each field is
        // moved out once.
        unsafe {
            drop(core::ptr::read(&this.source));
            ManuallyDrop::take(&mut this.io)
        }
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        // The registration goes before the descriptor closes.
        self.source.retire();
        // SAFETY: dropped once, here.
        unsafe { ManuallyDrop::drop(&mut self.io) };
    }
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
