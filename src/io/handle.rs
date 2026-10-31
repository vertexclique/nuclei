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
//! nuclei makes no descriptor of its own for a handle: every call, the
//! driver's, a ring's and a job's of the blocking pool, runs on the handle's
//! own descriptor. A call made away from the driver (a file's read or write
//! on the pool, a `send_file` chunk, a ring's attach) holds a lease on the
//! object for as long as it may use the number: a handle dropped meanwhile
//! hands its object to the source, and the last lease closes it; `get_mut`
//! and `into_inner` wait for the leases. So a process with no descriptor to
//! spare does everything std's would.
//!
//! # Bytes written behind
//!
//! A write is accepted into the handle's write-behind, as the kernel
//! accepts bytes into a socket's buffer, and the driver writes it out. Those
//! bytes go out whatever happens to the handle, as std's do: `get_mut` and
//! `into_inner` wait until the kernel took them, and a handle dropped before
//! that leaves its object to a task, which delivers them (for at most the
//! runtime's [`write_behind_linger`](crate::runtime::Builder::write_behind_linger))
//! and then closes it. A runtime that drops waits for those tasks (see
//! `rt::linger`), and none starts a runtime to run them.

/// How long a dropped handle's unsent bytes may take to reach the kernel, by
/// default (see [`Builder::write_behind_linger`](crate::runtime::Builder::write_behind_linger)).
pub(crate) const LINGER: Duration = Duration::from_secs(60);

use core::fmt;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use std::io;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{
    AsHandle, AsRawHandle, AsRawSocket, AsSocket, BorrowedHandle, BorrowedSocket, IntoRawHandle,
    IntoRawSocket, RawHandle, RawSocket,
};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::raw::Raw;
use super::source::{Kind, Source};
use super::sys;
use crate::fs::file::FileIo;

mod sealed {
    /// Only nuclei says what an [`IoObject`](super::IoObject) is.
    pub trait Sealed {}
}

/// An IO object a [`Handle`] takes over: on unix anything with a file
/// descriptor; on Windows std's `TcpStream`, `TcpListener`, `UdpSocket` and
/// `File`.
pub trait IoObject: sealed::Sealed {
    /// The descriptor (unix), the socket or the file handle (Windows) the
    /// handle's IO runs on.
    #[doc(hidden)]
    fn raw_io(&self) -> Raw;
}

#[cfg(unix)]
impl<T: AsFd> sealed::Sealed for T {}

#[cfg(unix)]
impl<T: AsFd> IoObject for T {
    fn raw_io(&self) -> Raw {
        self.as_fd().as_raw_fd()
    }
}

macro_rules! windows_sockets {
    ($($ty:ty),*) => {$(
        #[cfg(windows)]
        impl sealed::Sealed for $ty {}

        #[cfg(windows)]
        impl IoObject for $ty {
            fn raw_io(&self) -> Raw {
                self.as_raw_socket()
            }
        }
    )*};
}

windows_sockets!(
    std::net::TcpStream,
    std::net::TcpListener,
    std::net::UdpSocket
);

macro_rules! windows_handles {
    ($($ty:ty),*) => {$(
        #[cfg(windows)]
        impl sealed::Sealed for $ty {}

        #[cfg(windows)]
        impl IoObject for $ty {
            fn raw_io(&self) -> Raw {
                self.as_raw_handle() as usize as Raw
            }
        }
    )*};
}

windows_handles!(
    std::fs::File,
    std::process::ChildStdin,
    std::process::ChildStdout,
    std::process::ChildStderr
);

/// An IO object (a socket, a pipe, a file) whose operations run on nuclei's
/// shards.
///
/// `Send` and `Sync` exactly when the object is.
///
/// ```
/// use std::net::TcpListener;
/// use nuclei::Handle;
///
/// let listener = Handle::new(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
/// assert!(listener.local_addr().unwrap().port() > 0);
/// ```
pub struct Handle<T> {
    io: ManuallyDrop<T>,
    pub(crate) source: Arc<Source>,
    /// A regular file's IO state, made on its first operation.
    pub(crate) file: OnceLock<Arc<FileIo>>,
}

impl<T: IoObject> Handle<T> {
    /// Takes `io` over: what kind of object it is (a socket, a file, a
    /// pipe) decides how its IO runs. On unix it is made non-blocking.
    ///
    /// # Errors
    ///
    /// When the object cannot be inspected or set up.
    pub fn new(io: T) -> io::Result<Handle<T>> {
        let kind = sys::kind_of(io.raw_io())?;
        Self::adopt(io, kind)
    }

    /// Takes over `io`, an object of `kind` in blocking mode or not.
    pub(crate) fn adopt(io: T, kind: Kind) -> io::Result<Handle<T>> {
        #[cfg(unix)]
        {
            let fd = io.raw_io();
            sys::set_nonblocking(fd)?;
            if !matches!(kind, Kind::Bytes | Kind::File) {
                sys::set_nosigpipe(fd);
            }
        }
        Ok(Self::from_nonblocking(io, kind))
    }

    /// Takes over `io`, an object of `kind` set up already (an accepted
    /// connection, a socket nuclei made).
    pub(crate) fn from_nonblocking(io: T, kind: Kind) -> Handle<T> {
        let source = Source::new(io.raw_io(), kind);
        Handle {
            io: ManuallyDrop::new(io),
            source,
            file: OnceLock::new(),
        }
    }

    /// A regular file's IO state, made now on its first use, on a lease of
    /// the handle's own descriptor.
    pub(crate) fn file(&self) -> io::Result<&Arc<FileIo>> {
        if let Some(file) = self.file.get() {
            return Ok(file);
        }
        let file = FileIo::new(self.source().lease()?)?;
        Ok(self.file.get_or_init(|| file))
    }

    /// The source, with the object's descriptor in place: after a
    /// `get_mut` it may be another one.
    pub(crate) fn source(&self) -> &Arc<Source> {
        if self.source.is_retired() {
            self.source.revive(self.io.raw_io());
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
    /// first (this waits for them), then the handle's IO stops, and every
    /// call still out on its descriptor ends; its next operation takes the
    /// object as it is then (another descriptor discards what the old one
    /// left).
    pub fn get_mut(&mut self) -> &mut T {
        self.deliver_unsent();
        self.hand_file_back();
        self.source.retire();
        &mut self.io
    }

    /// The IO object, back from the handle, once the bytes written behind
    /// reached the kernel and every call still out on its descriptor ended
    /// (this waits for them). Bytes the handle read ahead and had not handed
    /// out are dropped with it.
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
    /// Waits until the kernel took what the write-behind holds (or the
    /// runtime's linger passed): on a shard, its other tasks run meanwhile.
    fn deliver_unsent(&self) {
        if self.source.has_unsent() {
            let source = &self.source;
            let _ = crate::sync::internal::blocking(crate::time::timeout(
                crate::rt::linger::time(),
                core::future::poll_fn(|cx| source.poll_drain(cx)),
            ));
        }
        if let Some(file) = self.file.get().filter(|f| f.has_unsent()) {
            let _ = crate::sync::internal::blocking(crate::time::timeout(
                crate::rt::linger::time(),
                core::future::poll_fn(|cx| file.poll_flush(cx)),
            ));
        }
    }

    /// Closes the object now, whatever the write-behind holds: the source's
    /// close, with no linger (`Source::close`).
    pub(crate) fn close_now(self) {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: `this` is never used or dropped again; each field is moved
        // out once.
        let (io, source, file) = unsafe {
            (
                ManuallyDrop::take(&mut this.io),
                core::ptr::read(&this.source),
                core::ptr::read(&this.file),
            )
        };
        drop(file);
        source.close(io);
    }
}

impl<T> Drop for Handle<T> {
    /// Never waits: the source closes the object once no call can use its
    /// descriptor (at once, or as the last lease or the driver lets go of
    /// it).
    fn drop(&mut self) {
        // A file's writes in flight hold leases on its descriptor; what waits
        // behind them is handed to a task, the file's state with its lease.
        if let Some(file) = self.file.take().filter(|f| f.has_unsent()) {
            crate::rt::linger::hand(async move {
                let _ = crate::time::timeout(
                    crate::rt::linger::time(),
                    core::future::poll_fn(|cx| file.poll_flush(cx)),
                )
                .await;
            });
        }
        // SAFETY: taken once, here; the field is never used again.
        let io = unsafe { ManuallyDrop::take(&mut self.io) };
        let Some(own) = self
            .source
            .raw_fd()
            .ok()
            .filter(|_| self.source.has_unsent())
        else {
            // The registration goes before the descriptor closes.
            self.source.close(io);
            return;
        };
        // The object itself stays with a task until the unsent bytes are
        // out, and closes then: the drain needs no descriptor of its own
        // (and on Windows, closing a socket would cancel its sends).
        self.source.linger(own);
        crate::rt::linger::hand(linger(Arc::clone(&self.source), Kept::new(io)));
    }
}

/// Delivers what a dropped handle's write-behind holds through `kept`, its
/// object itself, then closes it.
async fn linger(source: Arc<Source>, kept: Kept) {
    let drain = core::future::poll_fn(|cx| source.poll_drain(cx));
    let _ = crate::time::timeout(crate::rt::linger::time(), drain).await;
    source.close(kept);
}

/// A dropped handle's object, kept open while its unsent bytes go out.
struct Kept {
    ptr: core::ptr::NonNull<()>,
    drop: unsafe fn(core::ptr::NonNull<()>),
}

// SAFETY: a handle does IO only on std's sockets and files, all of them
// `Send` (see the module docs), and only a handle with bytes written behind
// lingers; the type is erased because `Drop` cannot say so.
unsafe impl Send for Kept {}

impl Kept {
    fn new<T>(io: T) -> Self {
        /// Drops the `T` boxed at `ptr`.
        ///
        /// # Safety
        ///
        /// `ptr` came from `Box::<T>::leak`, and is dropped once.
        unsafe fn drop_boxed<T>(ptr: core::ptr::NonNull<()>) {
            // SAFETY: the caller's.
            drop(unsafe { Box::from_raw(ptr.cast::<T>().as_ptr()) });
        }
        Self {
            ptr: core::ptr::NonNull::from(Box::leak(Box::new(io))).cast(),
            drop: drop_boxed::<T>,
        }
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        // SAFETY: made by `new`, dropped once, here.
        unsafe { (self.drop)(self.ptr) };
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

#[cfg(unix)]
impl<T: AsFd> AsFd for Handle<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.as_fd()
    }
}

#[cfg(unix)]
impl<T: AsRawFd> AsRawFd for Handle<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.io.as_raw_fd()
    }
}

#[cfg(unix)]
impl<T: IntoRawFd> IntoRawFd for Handle<T> {
    fn into_raw_fd(self) -> RawFd {
        self.into_inner().into_raw_fd()
    }
}

#[cfg(windows)]
impl<T: AsSocket> AsSocket for Handle<T> {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.io.as_socket()
    }
}

#[cfg(windows)]
impl<T: AsRawSocket> AsRawSocket for Handle<T> {
    fn as_raw_socket(&self) -> RawSocket {
        self.io.as_raw_socket()
    }
}

#[cfg(windows)]
impl<T: IntoRawSocket> IntoRawSocket for Handle<T> {
    fn into_raw_socket(self) -> RawSocket {
        self.into_inner().into_raw_socket()
    }
}

#[cfg(windows)]
impl<T: AsHandle> AsHandle for Handle<T> {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.io.as_handle()
    }
}

#[cfg(windows)]
impl<T: AsRawHandle> AsRawHandle for Handle<T> {
    fn as_raw_handle(&self) -> RawHandle {
        self.io.as_raw_handle()
    }
}

#[cfg(windows)]
impl<T: IntoRawHandle> IntoRawHandle for Handle<T> {
    fn into_raw_handle(self) -> RawHandle {
        self.into_inner().into_raw_handle()
    }
}
