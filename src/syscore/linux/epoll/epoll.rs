use crate::sys::epoll::*;
use futures::channel::oneshot;
use lever::prelude::*;
use pin_utils::unsafe_pinned;
use ahash::{HashMap, HashMapExt};
use std::future::Future;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fs::File, time::Duration};

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
use std::mem;
use std::os::unix::net::SocketAddr as UnixSocketAddr;

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

pub(crate) fn shim_recv_from<A: AsRawFd>(
    fd: A,
    buf: &mut [u8],
    flags: libc::c_int,
) -> io::Result<(usize, SockAddr)> {
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
            mem::size_of::<libc::c_char>(),
        )
    }[1] as libc::c_char;

    match (len, abst_sock_ident) {
        // NOTE: (vertexclique): If it is abstract socket, sa is greater than
        // sa_family_t, in that case assign len as the sa_family_t size.
        // https://man7.org/linux/man-pages/man7/unix.7.html
        (sa, 0) if sa != 0 && sa > mem::size_of::<libc::sa_family_t>() as libc::socklen_t => {
            len = mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
        }
        // If unnamed socket, then addr is always zero,
        // assign the offset reserved difference as length.
        (0, _) => {
            let base = &addr as *const _ as usize;
            let path = &addr.sun_path as *const _ as usize;
            let sun_path_offset = path - base;
            len = sun_path_offset as libc::socklen_t;
        }

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
            len as usize,
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

    /// Event trigger
    event_fd: TTas<File>,

    /// Registered events of IOs
    registered: TTas<HashMap<RawFd, i32>>,

    /// Hashmap for holding interested concrete completion callbacks
    completions: TTas<HashMap<RawFd, CompletionList>>,
}

impl SysProactor {
    pub(crate) fn new() -> io::Result<SysProactor> {
        let epoll_fd: i32 = epoll_create1()?;
        let event_fd: i32 = syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
        let event_fd_raw = event_fd;
        let event_fd = unsafe { File::from_raw_fd(event_fd) };
        let proactor = SysProactor {
            epoll_fd,
            event_fd: TTas::new(event_fd),
            registered: TTas::new(HashMap::new()),
            completions: TTas::new(HashMap::new()),
        };

        let ev = &mut EpollEvent::new(libc::EPOLLIN as _, 0 as u64);
        epoll_ctl(
            proactor.epoll_fd,
            EpollOp::EpollCtlAdd,
            event_fd_raw,
            Some(ev),
        )?;

        Ok(proactor)
    }

    fn default_flags(&self) -> EpollFlags {
        // Don't use RDHUP as registered with EPOLLIN.
        // Not all data might read with that.
        libc::EPOLLHUP | libc::EPOLLPRI | libc::EPOLLET
    }

    pub fn register(&self, fd: RawFd, events: i32) -> io::Result<()> {
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        let ev = &mut EpollEvent::new(events | self.default_flags(), fd as u64);
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, fd, Some(ev))
    }

    pub fn reregister(&self, fd: RawFd, events: i32) -> io::Result<()> {
        let ev = &mut EpollEvent::new(events | libc::EPOLLET, !0 as u64);
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, fd, Some(ev))
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlDel, fd, None)
    }

    pub fn wait(&self, max_event_size: usize, timeout: Option<Duration>) -> io::Result<usize> {
        // dbg!("WAIT");
        let mut events: Vec<EpollEvent> = Vec::with_capacity(max_event_size);
        events.resize(max_event_size, unsafe {
            MaybeUninit::zeroed().assume_init()
        });

        let timeout: isize = timeout.map_or(!0, |d| d.as_millis() as isize);
        let mut res = epoll_wait(self.epoll_fd, &mut events, timeout)? as usize;

        let mut ev_fd = self.event_fd.lock();
        for event in &events[0..res] {
            if event.data() == 0 {
                let mut buf = vec![0; 8];
                let _ = ev_fd.read(&mut buf);
                res -= 1;
                continue;
            }

            self.dequeue_events(event.data() as _, event.events());
        }

        Ok(res)
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        // dbg!("WAKE");
        self.event_fd.lock().write_all(&(1 as u64).to_ne_bytes())?;
        Ok(())
    }

    ///////

    pub(crate) fn register_io(&self, fd: RawFd, events: i32) -> io::Result<CompletionChan> {
        let mut events = events;
        let mut registered = self.registered.lock();
        let mut completions = self.completions.lock();

        if let Some(reged_evts) = registered.get_mut(&fd) {
            events |= *reged_evts;
            self.reregister(fd, events)?;
            *reged_evts = events;
        } else {
            // dbg!(events);
            self.register(fd, events)?;
            registered.insert(fd, events);
        }

        let (tx, rx) = oneshot::channel();
        let comp = completions.entry(fd).or_insert(Vec::new());

        comp.push((events, tx));

        Ok(CompletionChan { rx })
    }

    fn dequeue_events(&self, fd: RawFd, evts: i32) {
        let mut registered = self.registered.lock();
        let mut completions = self.completions.lock();

        // remove flags from interested events.
        let mut remove_regs = false;
        if let Some(reg_events) = registered.get_mut(&fd) {
            *reg_events &= !evts;
            if *reg_events == 0 {
                remove_regs = true;
            } else {
                let _ = self.reregister(fd, *reg_events);
            }
        }

        // send concrete completion and remove completion interested sources
        let mut ack_removal = false;
        if let Some(completions) = completions.get_mut(&fd) {
            let mut i = 0;
            while i < completions.len() {
                if completions[i].0 & evts != 0 {
                    let (_evts, sender) = completions.remove(i);
                    let _ = sender.send(evts);
                } else {
                    i += 1;
                }
            }

            if completions.is_empty() {
                ack_removal = true;
            }
        }

        if remove_regs {
            registered.remove(&fd);
            let _ = self.deregister(fd);
        }

        if ack_removal {
            completions.remove(&fd);
        }
    }
}

//////////////////////////////
//////////////////////////////

pub(crate) struct CompletionChan {
    rx: oneshot::Receiver<i32>,
}

impl CompletionChan {
    unsafe_pinned!(rx: oneshot::Receiver<i32>);
}

impl Future for CompletionChan {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.rx()
            .poll(cx)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been cancelled"))
    }
}
