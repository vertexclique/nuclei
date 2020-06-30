use std::mem::MaybeUninit;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::{fs::File, os::unix::net::UnixStream, collections::HashMap, time::Duration};
use futures::channel::oneshot;
use pin_utils::unsafe_pinned;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use lever::prelude::*;

macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

///////////////////
///////////////////

use socket2::SockAddr;
use std::os::unix::net::{SocketAddr as UnixSocketAddr};
use std::mem;

fn max_len() -> usize {
    // The maximum read limit on most posix-like systems is `SSIZE_MAX`,
    // with the man page quoting that if the count of bytes to read is
    // greater than `SSIZE_MAX` the result is "unspecified".
    //
    // On macOS, however, apparently the 64-bit libc is either buggy or
    // intentionally showing odd behavior by rejecting any read with a size
    // larger than or equal to INT_MAX. To handle both of these the read
    // size is capped on both platforms.
    if cfg!(target_os = "macos") {
        <libc::c_int>::max_value() as usize - 1
    } else {
        <libc::ssize_t>::max_value() as usize
    }
}

pub(crate) fn shim_recv_from<A: AsRawFd>(fd: A, buf: &mut [u8], flags: libc::c_int) -> io::Result<(usize, SockAddr)> {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut addrlen = mem::size_of_val(&storage) as libc::socklen_t;

    let n = syscall!(recvfrom(
            fd.as_raw_fd() as _,
            buf.as_mut_ptr() as *mut libc::c_void,
            std::cmp::min(buf.len(), max_len()),
            flags,
            &mut storage as *mut _ as *mut _,
            &mut addrlen,
        ))?;
    let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, addrlen) };
    Ok((n as usize, addr))
}

struct FakeUnixSocketAddr {
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
}

pub(crate) fn shim_to_af_unix(sockaddr: &SockAddr) -> io::Result<UnixSocketAddr> {
    let addr = unsafe { &*(sockaddr.as_ptr() as *const libc::sockaddr_un) };
    if addr.sun_family != libc::AF_UNIX as libc::sa_family_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "socket is not AF_UNIX type",
        ));
    }

    let mut len = sockaddr.len();
    let abst_sock_ident: libc::c_char = unsafe {
        std::slice::from_raw_parts(
            &addr.sun_path as *const _ as *const u8,
            mem::size_of::<libc::c_char>()
        )
    }[1] as libc::c_char;

    match (len, abst_sock_ident) {
        // NOTE: (vertexclique): If it is abstract socket, sa is greater than
        // sa_family_t, in that case assign len as the sa_family_t size.
        // https://man7.org/linux/man-pages/man7/unix.7.html
        (sa, 0) if sa != 0 && sa > mem::size_of::<libc::sa_family_t>() as libc::socklen_t => {
            len = mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
        },
        // If unnamed socket, then addr is always zero,
        // assign the offset reserved difference as length.
        (0, _) => {
            let base = &addr as *const _ as usize;
            let path = &addr.sun_path as *const _ as usize;
            let sun_path_offset = path - base;
            len = sun_path_offset as libc::socklen_t;
        },

        // Discard rest, they are not special.
        (_, _) => {}
    }

    let addr: UnixSocketAddr = unsafe {
        let mut init = MaybeUninit::<libc::sockaddr_un>::zeroed();
        // Safety: `*sockaddr` and `&init` are not overlapping and `*sockaddr`
        // points to valid memory.
        std::ptr::copy_nonoverlapping(
            sockaddr.as_ptr(),
            &mut init as *mut _ as *mut _,
            len as usize
        );

        // Safety: We've written the init addr above.
        std::mem::transmute(FakeUnixSocketAddr {
            addr: init.assume_init(),
            len: len as _,
        })
    };

    Ok(addr)
}

///////////////////
//// Socket Addr
///////////////////

type CompletionList = Vec<(i32, oneshot::Sender<i32>)>;

pub struct SysProactor {
    /// epoll_fd
    epoll_fd: RawFd,

    /// Waker trigger
    waker: TTas<UnixStream>,

    /// Registered events of IOs
    registered: TTas<HashMap<RawFd, i32>>,

    /// Hashmap for holding interested concrete completion callbacks
    completions: TTas<HashMap<RawFd, CompletionList>>
}

impl SysProactor {
    pub(crate) fn new() -> io::Result<SysProactor> {
        todo!()
    }

    pub fn register(&self, fd: RawFd, _key: i32) -> io::Result<()> {
        todo!()
    }

    pub fn reregister(&self, fd: RawFd, key: i32) -> io::Result<()> {
        todo!()
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        todo!()
    }

    pub fn wait(&self, max_event_size: usize, timeout: Option<Duration>) -> io::Result<usize> {
        todo!()
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        todo!()
    }

    ///////

    pub(crate) fn register_io(&self, fd: RawFd, evts: i32) -> io::Result<CompletionChan> {
        todo!()
    }

    fn dequeue_events(&self, fd: RawFd, evts: i32) {
        todo!()
    }
}

//////////////////////////////
//////////////////////////////

pub(crate) struct CompletionChan {
    recv: oneshot::Receiver<i32>,
}

impl CompletionChan {
    unsafe_pinned!(recv: oneshot::Receiver<i32>);
}

impl Future for CompletionChan {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.recv()
            .poll(cx)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been canceled"))
    }
}
