use crate::sys::event::{kevent_ts, kqueue, KEvent};
use futures::channel::oneshot;
use lever::prelude::*;
use pin_utils::unsafe_pinned;
use std::future::Future;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{collections::HashMap, os::unix::net::UnixStream, time::Duration};

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

type CompletionList = Vec<(usize, oneshot::Sender<usize>)>;

pub struct SysProactor {
    /// kqueue_fd
    kqueue_fd: RawFd,

    /// Waker socket half for read events (loop start)
    read_stream: TTas<UnixStream>,

    /// Waker socket half for write events (trigger)
    write_stream: UnixStream,

    /// Registered events of IOs
    registered: TTas<HashMap<RawFd, usize>>,

    /// Hashmap for holding interested concrete completion callbacks
    completions: TTas<HashMap<RawFd, CompletionList>>,
}

impl SysProactor {
    pub(crate) fn new() -> io::Result<SysProactor> {
        let kqueue_fd = kqueue()?;
        syscall!(fcntl(kqueue_fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
        let (read_stream, write_stream) = UnixStream::pair()?;
        read_stream.set_nonblocking(true)?;
        write_stream.set_nonblocking(true)?;
        let proactor = SysProactor {
            kqueue_fd,
            read_stream: TTas::new(read_stream),
            write_stream,
            registered: TTas::new(HashMap::new()),
            completions: TTas::new(HashMap::new()),
        };

        let rs = proactor.read_stream.lock();
        proactor.reregister(rs.as_raw_fd(), !0)?;
        drop(rs);

        Ok(proactor)
    }

    pub fn register(&self, fd: RawFd, _key: usize) -> io::Result<()> {
        // dbg!("REGISTER");
        let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
        syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        Ok(())
    }

    pub fn reregister(&self, fd: RawFd, key: usize) -> io::Result<()> {
        // dbg!("REREGISTER");
        // let mut read_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
        // let mut write_flags = libc::EV_ONESHOT | libc::EV_RECEIPT;
        let mut read_flags = libc::EV_CLEAR | libc::EV_RECEIPT;
        let mut write_flags = libc::EV_CLEAR | libc::EV_RECEIPT;

        read_flags |= libc::EV_ADD;
        write_flags |= libc::EV_ADD;

        let udata = key as _;
        let changelist = [
            KEvent::new(fd as _, libc::EVFILT_READ, read_flags, 0, 0, udata),
            KEvent::new(fd as _, libc::EVFILT_WRITE, write_flags, 0, 0, udata),
        ];
        let mut eventlist = changelist;
        kevent_ts(self.kqueue_fd, &changelist, &mut eventlist, None)?;
        for ev in &eventlist {
            // Explanation for ignoring EPIPE: https://github.com/tokio-rs/mio/issues/582
            let (flags, data) = (ev.flags(), ev.data());
            if (flags & libc::EV_ERROR) != 0
                && data != 0
                && data != libc::ENOENT as _
                && data != libc::EPIPE as _
            {
                return Err(io::Error::from_raw_os_error(data as _));
            }
        }

        Ok(())
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        // dbg!("DEREGISTER");
        let flags = libc::EV_DELETE | libc::EV_RECEIPT;
        let changelist = [
            KEvent::new(fd as _, libc::EVFILT_WRITE, flags, 0, 0, 0),
            KEvent::new(fd as _, libc::EVFILT_READ, flags, 0, 0, 0),
        ];
        let mut eventlist = changelist;
        kevent_ts(self.kqueue_fd, &changelist, &mut eventlist, None)?;
        for ev in &eventlist {
            let (flags, data) = (ev.flags(), ev.data());
            if (flags & libc::EV_ERROR != 0) && data != 0 && data != libc::ENOENT as _ {
                return Err(io::Error::from_raw_os_error(data as _));
            }
        }
        Ok(())
    }

    pub fn wait(&self, max_event_size: usize, timeout: Option<Duration>) -> io::Result<usize> {
        // dbg!("WAIT");
        let timeout = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });

        let mut events: Vec<KEvent> = Vec::with_capacity(max_event_size);
        events.resize(max_event_size, unsafe {
            MaybeUninit::zeroed().assume_init()
        });
        let mut events: Box<[KEvent]> = events.into_boxed_slice();

        // dbg!("SENDING EVENT");
        let res = kevent_ts(self.kqueue_fd, &[], &mut events, timeout)? as isize;
        // dbg!(res);
        // dbg!("EVENT FINISH");

        let mut res = res as usize;
        let mut rs = self.read_stream.lock();

        for event in &events[0..res] {
            let (flags, data) = (event.flags(), event.data());
            if (flags & libc::EV_ERROR) != 0
                && data != 0
                && data != libc::ENOENT as _
                && data != libc::EPIPE as _
            {
                return Err(io::Error::from_raw_os_error(data as _));
            } else {
                if event.ident() == rs.as_raw_fd() as _ {
                    let _ = rs.read(&mut [0; 64]);
                    // dbg!("READ AFTER");

                    // Skip waker
                    res -= 1;

                    // ignore to process more.
                    continue;
                } else {
                    // Read user registered entries
                    let _ = rs.read(&mut [0; 64]);
                    // dbg!("READ ACTUAL");
                }
            }

            // dbg!(event.ident());

            self.dequeue_events(event.ident() as _, event.flags() as _)
        }
        // self.reregister(rs.as_raw_fd(), !0)?;

        drop(rs);

        Ok(res)
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        // dbg!("WAKE");
        let _ = (&self.write_stream).write(&[1]);
        Ok(())
    }

    ///////

    pub(crate) fn register_io(&self, fd: RawFd, evts: usize) -> io::Result<CompletionChan> {
        let mut evts = evts;
        let mut registered = self.registered.lock();
        let mut completions = self.completions.lock();

        if let Some(reged_evts) = registered.get_mut(&fd) {
            evts |= *reged_evts;
            // dbg!(evts);
            self.reregister(fd, evts)?;
            *reged_evts = evts;
        } else {
            self.register(fd, evts)?;
            registered.insert(fd, evts);
        }

        let (tx, rx) = oneshot::channel();
        let comp = completions.entry(fd).or_insert(Vec::new());

        comp.push((evts, tx));

        Ok(CompletionChan { rx })
    }

    fn dequeue_events(&self, fd: RawFd, evts: usize) {
        // dbg!("DEQUEUE EVENTS");
        // acquire locks.
        let mut regs = self.registered.lock();
        let mut completions = self.completions.lock();

        // remove flags from interested events.
        let mut remove_regs = false;
        if let Some(reg_events) = regs.get_mut(&fd) {
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
            regs.remove(&fd);
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
    rx: oneshot::Receiver<usize>,
}

impl CompletionChan {
    unsafe_pinned!(rx: oneshot::Receiver<usize>);
}

impl Future for CompletionChan {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.rx()
            .poll(cx)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been cancelled"))
    }
}
