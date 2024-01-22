use core::mem::MaybeUninit;
use futures::channel::oneshot;
use lever::sync::prelude::*;
use pin_utils::unsafe_pinned;
use ahash::{HashMap, HashMapExt};
use std::future::Future;
use std::hash::BuildHasherDefault;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

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
use crossbeam_channel::{Receiver, Sender, unbounded};
use rustix::io_uring::IoringOp;
use rustix_uring::{CompletionQueue, IoUring, SubmissionQueue, Submitter, squeue::Entry as SQEntry, cqueue::Entry as CQEntry};
use rustix_uring::cqueue::{more, sock_nonempty};

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
//// uring impl
///////////////////

const QUEUE_LEN: u32 = 1 << 11;

pub struct SysProactor {
    sq: TTas<SubmissionQueue<'static>>,
    cq: TTas<CompletionQueue<'static>>,
    sbmt: TTas<Submitter<'static>>,
    submitters: TTas<HashMap<u64, Sender<i32>>>,
    submitter_id: AtomicU64,
}

pub type RingTypes = (
    SubmissionQueue<'static>,
    CompletionQueue<'static>,
    Submitter<'static>,
);

static mut IO_URING: Option<IoUring> = None;

impl SysProactor {
    pub(crate) fn new() -> io::Result<SysProactor> {
        unsafe {
            let ring = IoUring::builder()
                .setup_sqpoll(2)
                .build(QUEUE_LEN)
                .expect("nuclei: uring can't be initialized");

            IO_URING = Some(ring);

            let (submitter, sq, cq) = IO_URING.as_mut().unwrap().split();
            submitter.register_iowq_max_workers(&mut [QUEUE_LEN*8, 16])?;

            Ok(SysProactor {
                sq: TTas::new(sq),
                cq: TTas::new(cq),
                sbmt: TTas::new(submitter),
                submitters: TTas::new(HashMap::with_capacity(QUEUE_LEN as usize)),
                submitter_id: AtomicU64::default(),
            })
        }
    }

    pub(crate) fn register_files_sparse(&self, n: u32) -> io::Result<()> {
        let mut sbmt = self.sbmt.lock();
        Ok(sbmt.register_files_sparse(n)?)
    }

    pub(crate) fn register_io(
        &self,
        mut sqe: SQEntry,
    ) -> io::Result<CompletionChan> {
        let id = self.submitter_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = unbounded::<i32>();

        sqe = sqe.user_data(id);

        let mut subguard = self.submitters.lock();
        subguard.insert(id, tx);
        drop(subguard);

        let mut sq = self.sq.lock();
        unsafe {
            sq.push(&sqe).expect("nuclei: submission queue is full");
        }
        sq.sync();
        drop(sq);

        let sbmt = self.sbmt.lock();
        sbmt.submit()?;
        drop(sbmt);

        Ok(CompletionChan { rx })
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn wait(
        &self,
        max_event_size: usize,
        duration: Option<Duration>,
    ) -> io::Result<usize> {
        let mut cq = self.cq.lock();
        let mut acc: usize = 0;

        // issue cas barrier
        'sock: loop {
            cq.sync();
            while let Some(cqe) = cq.next() {
                if more(cqe.flags()) {
                    self.cqe_completion_multi(&cqe)?;
                } else {
                    self.cqe_completion_single(&cqe)?;
                }
                acc+=1;

                if !sock_nonempty(cqe.flags()) || !more(cqe.flags()) {
                    break 'sock;
                }
            }
        }

        Ok(acc)
    }

    fn cqe_completion_multi(&self, cqe: &CQEntry) -> io::Result<()> {
        let udata = cqe.user_data();
        let res: i32 = cqe.result();

        let mut sbmts = self.submitters.lock();
        sbmts.get(&udata).map(|s| s.send(res));
        // if atomics are going to be wrapped, channel will be reinserted.
        // which is ok.

        Ok(())
    }

    fn cqe_completion_single(&self, cqe: &CQEntry) -> io::Result<()> {
        let udata = cqe.user_data();
        let res: i32 = cqe.result();

        let mut sbmts = self.submitters.lock();
        let x = sbmts.remove(&udata);
        x.map(|s| s.send(res));

        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct CompletionChan {
    rx: Receiver<i32>,
}

impl CompletionChan {
    pub fn get_rx(&self) -> Receiver<i32> {
        self.rx.clone()
    }
}

impl CompletionChan {
    unsafe_pinned!(rx: Receiver<i32>);
}

impl Future for CompletionChan {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.rx();
        Poll::Ready(this.recv()
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been cancelled")))
    }
}
