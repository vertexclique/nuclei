use std::io;
use iou::{IoUring, SubmissionQueue, CompletionQueue, SubmissionQueueEvent, CompletionQueueEvent};
use lever::sync::prelude::*;
use std::collections::HashMap;
use futures::channel::oneshot;
use pin_utils::unsafe_pinned;
use std::future::Future;
use core::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, RawFd, FromRawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};

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
//// uring impl
///////////////////

// const MANUAL_TIMEOUT: u64 = u64::max_value() - 1;
const MANUAL_TIMEOUT: u64 = -2 as _;
const QUEUE_LEN: u32 = 1 << 8;
// const QUEUE_LEN: u32 = 1 << 0;

pub struct SysProactor {
    ring: TTas<IoUring>,
    submitters: TTas<HashMap<u64, oneshot::Sender<i32>>>,
    submitter_id: AtomicU64,
}

fn submitter<T>(sq: &mut SubmissionQueue<'_>, mut ring_sub: impl FnMut(&mut SubmissionQueueEvent<'_>) -> T) -> Option<T> {
    // dbg!("SUBMITTER");
    let mut sqe = match sq.next_sqe() {
        Some(sqe) => sqe,
        None => {
            if sq.submit().is_err() {
                return None;
            }
            sq.next_sqe()?
        }
    };

    Some(ring_sub(&mut sqe))
}

impl SysProactor {
    pub(crate) fn new() -> io::Result<SysProactor> {
        let mut ring = IoUring::new(QUEUE_LEN)?;

        Ok(SysProactor {
            ring: TTas::new(ring),
            submitters: TTas::new(HashMap::default()),
            submitter_id: AtomicU64::default()
        })
    }

    pub(crate) fn register_io(&self, mut io_submit: impl FnMut(&mut SubmissionQueueEvent<'_>)) -> io::Result<CompletionChan> {
        dbg!("REGISTER IO");
        let sub_comp = {
            let mut r = self.ring.lock();
            let mut sq = r.sq();

            let cc = submitter(&mut sq, |sqe| {
                // dbg!("SUBMITTER");
                let mut id = self.submitter_id.fetch_add(1, Ordering::Relaxed);
                if id == MANUAL_TIMEOUT {
                    id = self.submitter_id.fetch_add(1, Ordering::Relaxed);
                }
                let (tx, rx) = oneshot::channel();

                dbg!("SUBMITTER", id);
                sqe.set_user_data(id);
                io_submit(sqe);

                {
                    let mut subguard = self.submitters.lock();
                    subguard.insert(id, tx);
                    dbg!("INSERTED", id);
                }
                sqe.set_user_data(id);

                CompletionChan { rx }
            }).map(|c| {
                let submitted_io_evcount = sq.submit().unwrap();
                dbg!(submitted_io_evcount);

                c
            });

            cc
        };

        // dbg!(sub_comp.is_none());

        sub_comp.ok_or(io::Error::from(io::ErrorKind::WouldBlock))
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        // dbg!("WAKE");
        {
            let mut r = self.ring.lock();
            let mut sq = r.sq();

            let res = submitter(&mut sq, |sqe| {
                // dbg!("submit – timeout remove");
                unsafe {
                    let sqep = sqe.raw_mut();
                    sqep.user_data = MANUAL_TIMEOUT;
                    uring_sys::io_uring_prep_timeout_remove(sqep, MANUAL_TIMEOUT, 0);
                }
                // sqe.set_user_data(MANUAL_TIMEOUT);
            }).map(|c| {
                sq.submit().unwrap();
                c
            });

            res.ok_or(io::Error::from(io::ErrorKind::WouldBlock))?
        }

        Ok(())
    }

    pub(crate) fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        // dbg!("WAIT");
        let evcount = (max_event_size as isize).signum().abs() as usize;

        // dbg!("CQE POLL");
        match self.cqe_poll(evcount as _, duration) {
            Ok(_) => (),
            Err(ref e) if e.raw_os_error().unwrap() == libc::ETIME => return Ok(0),
            Err(ref e) if e.raw_os_error().unwrap() == libc::EAGAIN => return Ok(0),
            Err(e) => return Err(e),
        }
        // dbg!("RECEIVED");

        let mut r = self.ring.lock();
        let mut cq = r.cq();

        let mut acquired = 0;
        for _ in 0..max_event_size {
            // dbg!(cq.ready());
            if cq.ready() == 0 {
                break;
            }

            while let Some(cqe) = cq.peek_for_cqe() {
            // while let Ok(cqe) = cq.wait_for_cqe() {
                // maybe_cqe = Some(cqe);
                let udata = cqe.user_data();
                // dbg!("Fetched cqe", udata);
                let res = cqe.raw_result() as i32;
                if udata == MANUAL_TIMEOUT {
                    continue;
                }
                // dbg!("OTHER", udata);

                acquired += 1;

                self.submitters.lock()
                    .remove(&udata)
                    .map(|s| s.send(res));
            }
            // else { break; }
        }

        Ok(acquired)
    }

    fn cqe_poll(&self, evcount: u32, duration: Option<Duration>) -> io::Result<()> {
        let mut cqep = std::ptr::null_mut();
        let mut r = self.ring.lock();
        let (mut sq, mut cq, reg) = r.queues();

        let res = unsafe {
            if let Some(duration) = duration {
                let mut timeout_spec: _ = uring_sys::__kernel_timespec {
                    tv_sec:  duration.as_secs() as _,
                    tv_nsec: duration.subsec_nanos() as _,
                };

                submitter(&mut sq, |sqe| {
                    let sqep = sqe.raw_mut();
                    sqep.user_data = MANUAL_TIMEOUT;
                    uring_sys::io_uring_prep_timeout(sqep, &mut timeout_spec, evcount as _, 0);
                });
                sq.submit().unwrap();
            }
            uring_sys::io_uring_wait_cqe_nr(r.raw_mut() as *mut _, &mut cqep, evcount as _)
        };
        if res < 0 {
            return Err(io::Error::from_raw_os_error(-res));
        }


        Ok(())
    }
}

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
