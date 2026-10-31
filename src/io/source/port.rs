//! A source's operations on its shard's I/O completion port (Windows): how
//! a side's [`Want`] becomes an overlapped call, where its memory lives
//! while the kernel has it, and how its packet becomes an [`Outcome`].
//!
//! A port takes operations from any thread, so the side's holder issues
//! them itself; the port's shard dequeues their packets. A side has one
//! operation out at a time, so each side has one [`Slot`] in the source:
//! the `OVERLAPPED` the kernel completes (its address is the slot's, and is
//! what a packet names), the buffer descriptor, the address storage, the
//! accepted socket's address room, and the operation's memory. Nothing is
//! allocated per operation. From the call to the dequeue the kernel owns
//! the slot and a strong reference to the source; the dequeuing thread
//! takes them back, hands the outcome to the side through its mailbox, and
//! the side issues nothing on the slot before it has applied that outcome.
//!
//! A call that fails at once queues no packet: the issuer finishes the
//! operation itself, as a completion with that error.
//!
//! A TCP read the pool has no buffer for waits for its bytes in a zero-byte
//! receive, which holds no memory (see `read::ops`), and a stream write the
//! pool has no buffer for sends from its slot's own room (`ROOM`).
//!
//! A byte stream that is not a socket (a pipe, a console) is read and
//! written with `ReadFile` and `WriteFile`. One opened for overlapped IO is
//! bound to the port like a socket. One opened for synchronous IO cannot
//! complete on a port: its call is made by a job of the blocking pool, on a
//! copy of the handle, and the job brings its end to the port as a packet,
//! so the shard takes it as it takes the kernel's. Such a job is cancelled
//! with `CancelSynchronousIo` on its thread, under a handshake (see
//! [`Job`]) that keeps a cancel from ever meeting a later call of that
//! thread.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::io;
use std::os::windows::io::{AsRawHandle, AsRawSocket, OwnedHandle, OwnedSocket};
use std::sync::Arc;

use super::ops::{Answer, Dial, Done, Outcome, Want};
use super::{Addr, Kind, Source, Target};
use crate::buf::PoolBuf;
use crate::buf::owned::OwnedBuf;
use crate::io::raw::Raw;
use crate::io::side::After;
use crate::io::sys;
use crate::rt::park::iocp::{JOB, Port};
use crate::syscore::windows::{
    CancelIoEx, CancelSynchronousIo, DUPLICATE_SAME_ACCESS, DWORD, DuplicateHandle,
    ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_MORE_DATA, GetCurrentProcess,
    GetCurrentThread, GetLastError, HANDLE, MSG_PEEK, OVERLAPPED, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus, ReadFile, SOCKADDR, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR,
    WSA_OPERATION_ABORTED, WSABUF, WSARecv, WSARecvFrom, WSASend, WSASendTo, WriteFile,
    status_error, winsock,
};

/// Room for one address in an `AcceptEx` buffer: the largest address and
/// the 16 bytes the call asks for beside it.
const ACCEPT_ADDR: usize = size_of::<SOCKADDR_STORAGE>() + 16;

/// The bytes a read or a write the pool had no buffer for moves through its
/// slot's own room, the address room a stream's slot never uses otherwise.
const ROOM: usize = 2 * ACCEPT_ADDR;

/// The id of the read side's operation (the write side's is 1): what a
/// cancel names, the slot of that side.
const READ: u64 = 0;
const WRITE: u64 = 1;

/// What an operation holds while the kernel has it.
enum PortMem {
    Accept(OwnedSocket),
    /// An accept whose socket could not be made: it ends with that error.
    NoAccept,
    /// A zero-byte receive: it ends once bytes or the end came, and holds
    /// nothing.
    Ready,
    /// A read into the slot's own room.
    RecvRoom,
    /// A send of the bytes `from..to` of the slot's own room.
    SendRoom {
        from: usize,
        to: usize,
    },
    Recv(PoolBuf),
    RecvLent(OwnedBuf),
    RecvFrom(PoolBuf),
    RecvLentFrom(OwnedBuf),
    Connect(Dial),
    Send(PoolBuf),
    SendLent(OwnedBuf),
    SendTo(PoolBuf, Target),
    SendLentTo(OwnedBuf),
    /// The file a `TransmitFile` reads, kept open until its packet.
    SendFile(#[expect(dead_code, reason = "kept open until the packet")] Arc<OwnedHandle>),
}

/// A source's state on a port.
#[derive(Default)]
pub(crate) struct PortState {
    /// The read and the write side's operation slots.
    slots: [Slot; 2],
    /// For a byte stream opened for synchronous IO: the copy of its handle
    /// the jobs making its calls hold. Set as the source registers, with
    /// both sides held; read with one.
    sync: UnsafeCell<Option<Arc<OwnedHandle>>>,
}

/// A side's operation slot.
#[repr(C)]
pub(crate) struct Slot {
    /// First, so a packet's `OVERLAPPED` pointer is the slot's.
    ov: UnsafeCell<OVERLAPPED>,
    inner: UnsafeCell<Inner>,
    /// The slot's blocking job, if its operation is one (see [`Job`]).
    job: Job,
}

struct Inner {
    /// The strong reference held for the kernel while the operation is out.
    owner: *const Source,
    /// The port the operation was issued to, whose count it is in.
    issued_on: Option<Arc<Port>>,
    read: bool,
    epoch: u32,
    mem: Option<PortMem>,
    buf: WSABUF,
    flags: DWORD,
    addr: SOCKADDR_STORAGE,
    addr_len: i32,
    accept: [u8; 2 * ACCEPT_ADDR],
    /// How a blocking job's call ended, for its packet.
    job_code: Option<i32>,
}

// SAFETY: a slot is touched by the side's holder before the call, by the
// kernel (or a job) while the operation is out, and by the dequeuing thread
// after the packet; each hands it on through a synchronizing step (the
// call or the job's spawn, the port's dequeue, the side's mailbox). The
// job word is atomic.
unsafe impl Send for Slot {}
// SAFETY: as for `Send`.
unsafe impl Sync for Slot {}

// SAFETY: the handle copy is written only with both sides held and read
// only with one held; the slots are `Sync`.
unsafe impl Send for PortState {}
// SAFETY: as for `Send`.
unsafe impl Sync for PortState {}

impl Default for Slot {
    fn default() -> Self {
        Self {
            ov: UnsafeCell::new(OVERLAPPED::zeroed()),
            inner: UnsafeCell::new(Inner {
                owner: core::ptr::null(),
                issued_on: None,
                read: false,
                epoch: 0,
                mem: None,
                buf: WSABUF {
                    len: 0,
                    buf: core::ptr::null_mut(),
                },
                flags: 0,
                addr: SOCKADDR_STORAGE::zeroed(),
                addr_len: 0,
                accept: [0; 2 * ACCEPT_ADDR],
                job_code: None,
            }),
            job: Job::default(),
        }
    }
}

/// The word a blocking job and its canceller share. It holds `IDLE`,
/// `QUEUED` or `ABORTED`, or, once the job runs, the handle of its thread
/// (a multiple of four) with two flags: `CANCELLING` while a canceller is
/// at work, `OUT` once the job's call is over.
///
/// The job takes the word from `QUEUED` to its thread's handle before its
/// call; after it, it sets `OUT` (an `or`, which cannot fail) and frees the
/// word, which it cannot do while `CANCELLING` is set. A canceller sets
/// `CANCELLING` before `CancelSynchronousIo` and clears it after, and stops
/// once the cancel took or `OUT` is set; it tries again only while the job
/// is between taking the word and entering its call. So the thread a cancel
/// names is in that job, never in a later call, and a cancel is never lost.
/// proofs/tla/job checks this.
#[derive(Default)]
struct Job(AtomicUsize);

const IDLE: usize = 0;
const QUEUED: usize = 1;
/// Cancelled before it started: the job makes no call.
const ABORTED: usize = 2;
const CANCELLING: usize = 1;
const OUT: usize = 2;

impl Job {
    /// The job starts its call on the thread `me`: `false` if it was
    /// cancelled first.
    fn begin(&self, me: usize) -> bool {
        self.0
            .compare_exchange(QUEUED, me, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The job's call is over: once no canceller is at work, the word is
    /// free.
    fn end(&self, me: usize) {
        self.0.fetch_or(OUT, Ordering::AcqRel);
        while self
            .0
            .compare_exchange_weak(me | OUT, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    /// Cancels the job: one not started makes no call, and the call of one
    /// running ends (if its device lets a call be cancelled). Never waits
    /// for the job; tries again only while the job is between taking the
    /// word and entering its call, a few instructions. The caller holds
    /// the slot's side, so no other canceller is at work.
    fn cancel(&self) {
        loop {
            match self.0.load(Ordering::Acquire) {
                IDLE | ABORTED => return,
                QUEUED => {
                    if self
                        .0
                        .compare_exchange(QUEUED, ABORTED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
                // The call is over (or another canceller is at work).
                done if done & (CANCELLING | OUT) != 0 => return,
                thread => {
                    if self
                        .0
                        .compare_exchange(
                            thread,
                            thread | CANCELLING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    // SAFETY: the thread's handle is open while the word
                    // names it: its job cannot end while `CANCELLING` is set.
                    let cancelled = unsafe { CancelSynchronousIo(thread as HANDLE) } != 0;
                    let after = self.0.fetch_and(!CANCELLING, Ordering::AcqRel);
                    if cancelled || after & OUT != 0 {
                        return;
                    }
                    // Not in its call yet.
                    std::thread::yield_now();
                }
            }
        }
    }
}

/// A handle of the calling thread that another thread can use, made once
/// per thread, closed as the thread ends.
struct ThisThread(HANDLE);

impl Drop for ThisThread {
    fn drop(&mut self) {
        // SAFETY: the handle is ours, and no job of this thread runs now.
        unsafe { crate::syscore::windows::CloseHandle(self.0) };
    }
}

thread_local! {
    static THIS_THREAD: Option<ThisThread> = {
        let mut real: HANDLE = core::ptr::null_mut();
        // SAFETY: duplicates the pseudo-handle of this thread into a real
        // one of our own.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentThread(),
                GetCurrentProcess(),
                &mut real,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        // A handle's two low bits are always clear: the job word's flags
        // live there.
        (ok != 0 && real as usize >= 4 && real as usize & 3 == 0).then_some(ThisThread(real))
    };
}

/// The port a source's operations go to, as the thread holding its side
/// sees it: any thread issues them.
#[derive(Clone, Copy)]
pub(crate) enum PortIo<'a> {
    Here {
        source: &'a Arc<Source>,
    },
    /// The registration is under way on another thread: the side waits,
    /// and is woken once it is in place.
    NotYet,
}

/// Issues a source's operations, from the thread holding the side.
pub(crate) struct PortIssuer<'a> {
    source: &'a Arc<Source>,
    port: Arc<Port>,
}

impl<'a> PortIo<'a> {
    pub(crate) fn issuer(self, _after: &mut After) -> Option<PortIssuer<'a>> {
        match self {
            PortIo::Here { source } => source.home_held().map(|home| PortIssuer {
                source,
                port: Arc::clone(home.port()),
            }),
            PortIo::NotYet => None,
        }
    }

    /// The source's socket, for a call the side's holder makes on it.
    pub(crate) fn socket(self) -> Option<Raw> {
        match self {
            PortIo::Here { source } => match source.fd.load(Ordering::Acquire) {
                super::NO_FD => None,
                fd => Some(fd),
            },
            PortIo::NotYet => None,
        }
    }
}

fn socket_of(raw: Raw) -> SOCKET {
    raw as SOCKET
}

/// The answer of an operation that ended with Winsock or Win32 error
/// `code` (`None`: it ran) after `bytes`, an error counted for `stats`.
/// One cancelled after it took some bytes counts them, so none goes out
/// twice.
fn answer(code: Option<i32>, bytes: usize) -> Answer<usize> {
    match code {
        None => Answer::Done(Ok(bytes)),
        Some(WSA_OPERATION_ABORTED) if bytes == 0 => Answer::Undone { not_ready: false },
        Some(WSA_OPERATION_ABORTED) => Answer::Done(Ok(bytes)),
        Some(code) => {
            let err = io::Error::from_raw_os_error(code);
            crate::rt::counters::io_error(&err);
            Answer::Done(Err(err))
        }
    }
}

/// A read's answer: the end of a pipe or a file is its end of stream.
fn read_answer(code: Option<i32>, bytes: usize) -> Answer<usize> {
    match code {
        Some(ERROR_BROKEN_PIPE | ERROR_HANDLE_EOF) => Answer::Done(Ok(0)),
        code => answer(code, bytes),
    }
}

/// The result of an overlapped call: `Ok` when a packet will come (it is
/// pending, or done and queued), else the error of a call that queued none.
fn queued(ret: i32) -> Result<(), i32> {
    if ret != SOCKET_ERROR {
        return Ok(());
    }
    // SAFETY: reads this thread's last Winsock error.
    match unsafe { crate::syscore::windows::WSAGetLastError() } {
        ERROR_IO_PENDING => Ok(()),
        code => Err(code),
    }
}

/// As [`queued`], for a call that answers with a `BOOL`.
fn queued_bool(ok: i32) -> Result<(), i32> {
    queued(if ok == 0 { SOCKET_ERROR } else { 0 })
}

impl PortIssuer<'_> {
    /// Copies the front of `bufs` into the write slot's own room, as much as
    /// it holds, for a write the pool has no buffer for to send from
    /// (`Want::SendRoom`); returns how many bytes. The write side is held and
    /// has nothing out, so nothing else touches the room.
    pub(crate) fn fill_room(&self, bufs: &[std::io::IoSlice<'_>]) -> usize {
        let slot = &self.source.port.slots[WRITE as usize];
        // SAFETY: as above: the room is this side's holder's alone.
        let room = unsafe { &mut (*slot.inner.get()).accept };
        let mut n = 0;
        for buf in bufs {
            let k = buf.len().min(ROOM - n);
            room[n..n + k].copy_from_slice(&buf[..k]);
            n += k;
            if n == ROOM {
                break;
            }
        }
        n
    }

    /// Issues `want` for a side of a source of `kind` in `epoch`; returns
    /// the side's operation id.
    pub(crate) fn issue(&self, want: Want, kind: Kind, epoch: u32, _after: &mut After) -> u64 {
        let read = want.of_read();
        let id = if read { READ } else { WRITE };
        let slot = &self.source.port.slots[id as usize];
        // SAFETY: the side is held and its last operation's outcome was
        // applied (the side had none out): nothing else touches the slot.
        let inner = unsafe { &mut *slot.inner.get() };
        // SAFETY: as above, for the OVERLAPPED.
        unsafe { *slot.ov.get() = OVERLAPPED::zeroed() };
        let fd = self.source.fd.load(Ordering::Acquire);
        inner.read = read;
        inner.epoch = epoch;
        self.source.op_out();
        self.port.issued();
        inner.issued_on = Some(Arc::clone(&self.port));
        inner.owner = Arc::into_raw(Arc::clone(self.source));
        let started = if kind != Kind::Bytes {
            start(inner, slot.ov.get(), socket_of(fd), want, kind.datagrams())
        // SAFETY: a side is held, so the copy is not being written.
        } else if let Some(handle) = unsafe { (*self.source.port.sync.get()).clone() } {
            start_job(slot, inner, handle, Arc::clone(&self.port), want)
        } else {
            start_stream(inner, slot.ov.get(), fd as usize as HANDLE, want)
        };
        if let Err(code) = started {
            // No packet comes: the operation ends here, with the error.
            // SAFETY: the call did not take the slot.
            unsafe { finish(slot, Some(code), 0, None) };
        }
        id
    }
}

/// Makes the overlapped call for `want` on `socket` with `ov`, the memory
/// in `inner`; `datagrams` for a socket of datagrams. `Ok`: a packet will
/// come.
fn start(
    inner: &mut Inner,
    ov: *mut OVERLAPPED,
    socket: SOCKET,
    want: Want,
    datagrams: bool,
) -> Result<(), i32> {
    let null = core::ptr::null();
    let fail = |inner: &mut Inner, mem: PortMem, err: io::Error| -> Result<(), i32> {
        inner.mem = Some(mem);
        Err(err
            .raw_os_error()
            .unwrap_or(crate::syscore::windows::WSAEINVAL))
    };
    let want = match stream_memory(inner, want) {
        Ok(true) => {
            // A zero-byte receive of a datagram socket looks: one taken into
            // no room is cut to nothing and the rest of it lost (UDP's own
            // rule, WSARecvFrom's remarks), so MSG_PEEK leaves it queued, as
            // libuv's zero reads of UDP do (src/win/udp.c).
            let looks = datagrams && matches!(inner.mem, Some(PortMem::Ready));
            inner.flags = if looks { MSG_PEEK as DWORD } else { 0 };
            // SAFETY: the buffer and the flags live in the slot until the
            // packet; the kernel writes the buffer's spare room.
            return queued(unsafe {
                WSARecv(
                    socket,
                    &mut inner.buf,
                    1,
                    core::ptr::null_mut(),
                    &mut inner.flags,
                    ov,
                    null,
                )
            });
        }
        Ok(false) => {
            // SAFETY: the buffer lives in the slot until the packet; the
            // kernel only reads it.
            return queued(unsafe {
                WSASend(socket, &inner.buf, 1, core::ptr::null_mut(), 0, ov, null)
            });
        }
        Err(other) => other,
    };
    match want {
        NotStream::RecvFrom(mut buf) => {
            let spare = buf.spare();
            inner.buf = WSABUF {
                len: u32::try_from(spare.len()).unwrap_or(u32::MAX),
                buf: spare.as_mut_ptr(),
            };
            inner.flags = 0;
            inner.addr_len = size_of::<SOCKADDR_STORAGE>() as i32;
            inner.mem = Some(PortMem::RecvFrom(buf));
            // SAFETY: the buffer, the flags and the address room live in the
            // slot until the packet.
            queued(unsafe {
                WSARecvFrom(
                    socket,
                    &mut inner.buf,
                    1,
                    core::ptr::null_mut(),
                    &mut inner.flags,
                    core::ptr::from_mut(&mut inner.addr).cast::<SOCKADDR>(),
                    &mut inner.addr_len,
                    ov,
                    null,
                )
            })
        }
        NotStream::RecvLentFrom(mut buf) => {
            let spare = buf.spare_capacity_mut();
            inner.buf = WSABUF {
                len: u32::try_from(spare.len()).unwrap_or(u32::MAX),
                buf: spare.as_mut_ptr().cast(),
            };
            inner.flags = 0;
            inner.addr_len = size_of::<SOCKADDR_STORAGE>() as i32;
            inner.mem = Some(PortMem::RecvLentFrom(buf));
            // SAFETY: the buffer, the flags and the address room live in the
            // slot until the packet; the kernel writes the buffer's spare
            // room.
            queued(unsafe {
                WSARecvFrom(
                    socket,
                    &mut inner.buf,
                    1,
                    core::ptr::null_mut(),
                    &mut inner.flags,
                    core::ptr::from_mut(&mut inner.addr).cast::<SOCKADDR>(),
                    &mut inner.addr_len,
                    ov,
                    null,
                )
            })
        }
        NotStream::SendLentTo(buf, target) => {
            inner.buf = WSABUF {
                len: u32::try_from(buf.len()).unwrap_or(u32::MAX),
                buf: buf.as_ptr().cast_mut(),
            };
            let to = match &target {
                Target::Connected => None,
                Target::Inet(addr) => Some(sys::raw_addr(addr)),
            };
            inner.mem = Some(PortMem::SendLentTo(buf));
            match to {
                // SAFETY: the buffer lives in the slot until the packet; the
                // kernel only reads it.
                None => queued(unsafe {
                    WSASend(socket, &inner.buf, 1, core::ptr::null_mut(), 0, ov, null)
                }),
                Some((addr, len)) => {
                    inner.addr = addr;
                    // SAFETY: the buffer and the address live in the slot
                    // until the packet.
                    queued(unsafe {
                        WSASendTo(
                            socket,
                            &inner.buf,
                            1,
                            core::ptr::null_mut(),
                            0,
                            core::ptr::from_ref(&inner.addr).cast::<SOCKADDR>(),
                            len,
                            ov,
                            null,
                        )
                    })
                }
            }
        }
        NotStream::SendFile(file) => {
            let Some(transmit) = winsock::ext().ok().and_then(|ext| ext.transmit_file) else {
                return fail(
                    inner,
                    PortMem::SendFile(file.handle),
                    io::ErrorKind::Unsupported.into(),
                );
            };
            let raw = file.handle.as_raw_handle();
            inner.mem = Some(PortMem::SendFile(file.handle));
            // SAFETY: the slot's OVERLAPPED is ours until the packet; the offset is where the
            // file is read.
            unsafe {
                (*ov).Offset = file.offset as DWORD;
                (*ov).OffsetHigh = (file.offset >> 32) as DWORD;
            }
            // SAFETY: the socket is the source's, the file handle is kept in the slot until the
            // packet, and `ov` is the slot's.
            queued_bool(unsafe { transmit(socket, raw.cast(), file.len, 0, ov, null.cast(), 0) })
        }
        NotStream::Accept => {
            let made = winsock::ext().and_then(|ext| {
                let family = sys::family(socket as Raw)?;
                Ok((ext, sys::tcp_socket(family)?))
            });
            let (ext, conn) = match made {
                Ok(made) => made,
                Err(err) => return fail(inner, PortMem::NoAccept, err),
            };
            let accepted = conn.as_raw_socket() as SOCKET;
            inner.mem = Some(PortMem::Accept(conn));
            let mut bytes: DWORD = 0;
            // SAFETY: the accept socket and the address room live in the
            // slot until the packet.
            queued_bool(unsafe {
                (ext.accept_ex)(
                    socket,
                    accepted,
                    inner.accept.as_mut_ptr().cast::<c_void>(),
                    0,
                    ACCEPT_ADDR as DWORD,
                    ACCEPT_ADDR as DWORD,
                    &mut bytes,
                    ov,
                )
            })
        }
        NotStream::Connect(dial) => {
            let Dial::Inet(peer) = &dial;
            let family = if peer.is_ipv4() {
                crate::syscore::windows::AF_INET
            } else {
                crate::syscore::windows::AF_INET6
            };
            let ext = match winsock::ext()
                .and_then(|ext| sys::bind_any(socket as Raw, family).map(|()| ext))
            {
                Ok(ext) => ext,
                Err(err) => return fail(inner, PortMem::Connect(dial), err),
            };
            let (addr, len) = sys::raw_addr(peer);
            inner.addr = addr;
            inner.mem = Some(PortMem::Connect(dial));
            // SAFETY: the address lives in the slot until the packet.
            queued_bool(unsafe {
                (ext.connect_ex)(
                    socket,
                    core::ptr::from_ref(&inner.addr).cast::<SOCKADDR>(),
                    len,
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                    ov,
                )
            })
        }
        NotStream::SendTo(buf, target) => {
            let bytes = buf.filled();
            inner.buf = WSABUF {
                len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
                buf: bytes.as_ptr().cast_mut(),
            };
            let to = match &target {
                Target::Connected => None,
                Target::Inet(addr) => Some(sys::raw_addr(addr)),
            };
            inner.mem = Some(PortMem::SendTo(buf, target));
            match to {
                // SAFETY: as above.
                None => queued(unsafe {
                    WSASend(socket, &inner.buf, 1, core::ptr::null_mut(), 0, ov, null)
                }),
                Some((addr, len)) => {
                    inner.addr = addr;
                    // SAFETY: the buffer and the address live in the slot
                    // until the packet.
                    queued(unsafe {
                        WSASendTo(
                            socket,
                            &inner.buf,
                            1,
                            core::ptr::null_mut(),
                            0,
                            core::ptr::from_ref(&inner.addr).cast::<SOCKADDR>(),
                            len,
                            ov,
                            null,
                        )
                    })
                }
            }
        }
    }
}

/// The wants that are no stream's: a listener's, a datagram socket's, a
/// connect.
enum NotStream {
    Accept,
    RecvFrom(PoolBuf),
    RecvLentFrom(OwnedBuf),
    Connect(Dial),
    SendTo(PoolBuf, Target),
    SendLentTo(OwnedBuf, Target),
    SendFile(super::write::FileRange),
}

impl NotStream {
    /// Keeps the memory of a want no call was made for in the slot, for
    /// the failed operation to hand back.
    fn keep_in(self, inner: &mut Inner) {
        inner.mem = Some(match self {
            NotStream::Accept => PortMem::NoAccept,
            NotStream::RecvFrom(buf) => PortMem::RecvFrom(buf),
            NotStream::RecvLentFrom(buf) => PortMem::RecvLentFrom(buf),
            NotStream::SendLentTo(buf, _) => PortMem::SendLentTo(buf),
            NotStream::SendFile(file) => PortMem::SendFile(file.handle),
            NotStream::Connect(dial) => PortMem::Connect(dial),
            NotStream::SendTo(buf, target) => PortMem::SendTo(buf, target),
        });
    }
}

/// Points the slot's buffer descriptor at the bytes a stream's `want`
/// reads into (`Ok(true)`) or writes from (`Ok(false)`), the memory kept in
/// the slot; any other want comes back.
fn stream_memory(inner: &mut Inner, want: Want) -> Result<bool, NotStream> {
    let wsabuf = |ptr: *mut u8, len: usize| WSABUF {
        len: u32::try_from(len).unwrap_or(u32::MAX),
        buf: ptr,
    };
    match want {
        Want::Ready { read } => {
            // An empty slice's pointer, as a zero-byte call is made with.
            inner.buf = wsabuf(core::ptr::NonNull::dangling().as_ptr(), 0);
            inner.mem = Some(PortMem::Ready);
            Ok(read)
        }
        Want::RecvRoom => {
            inner.buf = wsabuf(inner.accept.as_mut_ptr(), ROOM);
            inner.mem = Some(PortMem::RecvRoom);
            Ok(true)
        }
        Want::SendRoom { from, to } => {
            let bytes = inner.accept.get_mut(from..to).unwrap_or_default();
            inner.buf = wsabuf(bytes.as_mut_ptr(), bytes.len());
            inner.mem = Some(PortMem::SendRoom { from, to });
            Ok(false)
        }
        Want::Recv(mut buf) => {
            let spare = buf.spare();
            inner.buf = wsabuf(spare.as_mut_ptr(), spare.len());
            inner.mem = Some(PortMem::Recv(buf));
            Ok(true)
        }
        Want::RecvLent(mut buf) => {
            let spare = buf.spare_capacity_mut();
            inner.buf = wsabuf(spare.as_mut_ptr().cast(), spare.len());
            inner.mem = Some(PortMem::RecvLent(buf));
            Ok(true)
        }
        Want::Send(buf) => {
            let bytes = buf.filled();
            inner.buf = wsabuf(bytes.as_ptr().cast_mut(), bytes.len());
            inner.mem = Some(PortMem::Send(buf));
            Ok(false)
        }
        Want::SendLent(buf, from) => {
            let bytes = buf.rest_from(from);
            inner.buf = wsabuf(bytes.as_ptr().cast_mut(), bytes.len());
            inner.mem = Some(PortMem::SendLent(buf));
            Ok(false)
        }
        Want::Accept => Err(NotStream::Accept),
        Want::RecvFrom(buf) => Err(NotStream::RecvFrom(buf)),
        Want::RecvLentFrom(buf) => Err(NotStream::RecvLentFrom(buf)),
        Want::SendFile(file) => Err(NotStream::SendFile(file)),
        Want::SendLentTo(buf, target) => Err(NotStream::SendLentTo(buf, target)),
        Want::Connect(dial) => Err(NotStream::Connect(dial)),
        Want::SendTo(buf, target) => Err(NotStream::SendTo(buf, target)),
    }
}

/// Makes the overlapped `ReadFile` or `WriteFile` of a stream's `want` on
/// `handle`, bound to the port. `Ok`: a packet will come.
fn start_stream(
    inner: &mut Inner,
    ov: *mut OVERLAPPED,
    handle: HANDLE,
    want: Want,
) -> Result<(), i32> {
    let mut done: DWORD = 0;
    match stream_memory(inner, want) {
        // SAFETY: the buffer lives in the slot until the packet; the kernel
        // writes its spare room.
        Ok(true) => queued_bool(unsafe {
            ReadFile(handle, inner.buf.buf.cast(), inner.buf.len, &mut done, ov)
        }),
        // SAFETY: as above; the kernel only reads it.
        Ok(false) => queued_bool(unsafe {
            WriteFile(handle, inner.buf.buf.cast(), inner.buf.len, &mut done, ov)
        }),
        // No stream asks for another operation.
        Err(other) => {
            other.keep_in(inner);
            Err(crate::syscore::windows::WSAEINVAL)
        }
    }
}

/// Hands a stream's `want` on a handle opened for synchronous IO to a job
/// of the blocking pool, which makes the call on its copy of the handle
/// and posts the packet to `port`. `Ok`: the packet will come.
fn start_job(
    slot: &Slot,
    inner: &mut Inner,
    handle: Arc<OwnedHandle>,
    port: Arc<Port>,
    want: Want,
) -> Result<(), i32> {
    let read = match stream_memory(inner, want) {
        Ok(read) => read,
        Err(other) => {
            other.keep_in(inner);
            return Err(crate::syscore::windows::WSAEINVAL);
        }
    };
    slot.job.0.store(QUEUED, Ordering::Release);
    let at = core::ptr::from_ref(slot) as usize;
    crate::runtime::spawn_blocking(move || {
        // SAFETY: the slot is in the source the operation's reference keeps
        // until its packet is taken, which this job posts last.
        let slot = unsafe { &*(at as *const Slot) };
        run_job(slot, &handle, read, &port);
    })
    .detach();
    Ok(())
}

/// A blocking job's body: the call, unless it was cancelled first, then
/// its packet.
fn run_job(slot: &Slot, handle: &OwnedHandle, read: bool, port: &Port) {
    // SAFETY: the job has the slot from its spawn to its packet.
    let inner = unsafe { &mut *slot.inner.get() };
    let me = THIS_THREAD
        .try_with(|t| t.as_ref().map_or(0, |t| t.0 as usize))
        .unwrap_or(0);
    let (code, bytes) = if me == 0 {
        // No handle a canceller could use: the call is not made.
        slot.job.0.store(IDLE, Ordering::Release);
        (Some(crate::syscore::windows::WSAENOBUFS), 0)
    } else if !slot.job.begin(me) {
        slot.job.0.store(IDLE, Ordering::Release);
        (Some(WSA_OPERATION_ABORTED), 0)
    } else {
        let h = handle.as_raw_handle().cast();
        let mut done: DWORD = 0;
        // SAFETY: the buffer is the slot's, as `stream_memory` set it; the
        // call blocks this thread until it ends.
        let ok = unsafe {
            if read {
                ReadFile(
                    h,
                    inner.buf.buf.cast(),
                    inner.buf.len,
                    &mut done,
                    core::ptr::null_mut(),
                )
            } else {
                WriteFile(
                    h,
                    inner.buf.buf.cast(),
                    inner.buf.len,
                    &mut done,
                    core::ptr::null_mut(),
                )
            }
        };
        // SAFETY: reads this thread's last error.
        let code = (ok == 0).then(|| unsafe { GetLastError() } as i32);
        slot.job.end(me);
        // A message longer than the buffer: the rest comes with the next.
        (code.filter(|&c| c != ERROR_MORE_DATA), done as usize)
    };
    inner.job_code = code;
    // SAFETY: a packet naming the slot, on a port the job keeps alive; the
    // slot is the dequeuing thread's from here on.
    unsafe { PostQueuedCompletionStatus(port.handle(), bytes as DWORD, JOB, slot.ov.get()) };
}

/// The packet of an operation, dequeued from a port.
///
/// # Safety
///
/// `entry` is a packet of an operation a [`PortIssuer`] issued: its
/// `OVERLAPPED` is a slot's, which the kernel is done with.
pub(crate) unsafe fn complete(entry: &OVERLAPPED_ENTRY, shard: Option<&crate::rt::shard::Shard>) {
    // SAFETY: the caller's: the slot is of a live source (its reference
    // was held for the kernel), and the kernel or the job is done with it.
    let slot = unsafe { &*entry.lpOverlapped.cast::<Slot>().cast_const() };
    let code = if entry.lpCompletionKey == JOB {
        // SAFETY: as above; the job wrote its end before the packet.
        unsafe { (*slot.inner.get()).job_code.take() }
    } else {
        status_error(entry.Internal as i32)
    };
    // SAFETY: as above.
    unsafe { finish(slot, code, entry.dwNumberOfBytesTransferred as usize, shard) };
}

/// Ends the operation of `slot`, which failed with `code` or ran (`None`)
/// over `bytes`: its memory and the source's reference come back, and the
/// outcome goes to the side.
///
/// # Safety
///
/// The kernel is done with the slot, and its operation was issued.
unsafe fn finish(
    slot: &Slot,
    code: Option<i32>,
    bytes: usize,
    shard: Option<&crate::rt::shard::Shard>,
) {
    // SAFETY: the caller's: the slot is this thread's until the outcome is
    // handed on below.
    let inner = unsafe { &mut *slot.inner.get() };
    // SAFETY: the reference the issuer leaked for the kernel.
    let source = unsafe { Arc::from_raw(inner.owner) };
    inner.owner = core::ptr::null();
    if let Some(port) = inner.issued_on.take() {
        port.completed();
    }
    // Every call sets the memory before it can end (see `start`).
    let Some(mem) = inner.mem.take() else {
        return;
    };
    let outcome = outcome(inner, mem, code, bytes);
    source.op_done(
        Done {
            outcome,
            epoch: inner.epoch,
            read: inner.read,
        },
        shard,
    );
}

/// The outcome of an operation on `mem`, from what the kernel left in the
/// slot.
fn outcome(inner: &mut Inner, mem: PortMem, code: Option<i32>, bytes: usize) -> Outcome {
    // Made once: an error is counted as it is made.
    let answer = match mem {
        PortMem::Ready | PortMem::RecvRoom | PortMem::Recv(_) | PortMem::RecvLent(_) => {
            read_answer(code, bytes)
        }
        _ => answer(code, bytes),
    };
    match mem {
        PortMem::Ready => Outcome::Ready(match answer {
            Answer::Done(result) => Answer::Done(result.map(drop)),
            Answer::Undone { not_ready } => Answer::Undone { not_ready },
        }),
        PortMem::RecvRoom => Outcome::RecvRoom(match answer {
            Answer::Done(result) => Answer::Done(result.map(|n| {
                let at = core::ptr::NonNull::from(&mut inner.accept).cast::<u8>();
                super::ops::Room::new(at, n.min(ROOM))
            })),
            Answer::Undone { not_ready } => Answer::Undone { not_ready },
        }),
        PortMem::SendRoom { from, to } => Outcome::SendRoom { from, to, answer },
        PortMem::Recv(mut buf) => {
            if let Answer::Done(Ok(n)) = answer {
                buf.set_filled(n);
            }
            Outcome::Recv(buf, answer)
        }
        PortMem::RecvLent(mut buf) => {
            if let Answer::Done(Ok(n)) = answer {
                // SAFETY: the kernel wrote the first `n` bytes of the spare
                // capacity.
                unsafe { buf.set_len(buf.len() + n) };
            }
            Outcome::RecvLent(buf, answer)
        }
        PortMem::RecvFrom(mut buf) => {
            let answer = match answer {
                Answer::Done(Ok(n)) => {
                    buf.set_filled(n);
                    // SAFETY: the kernel wrote the sender's address and its
                    // length into the slot.
                    let from = unsafe {
                        sys::socket_addr(
                            core::ptr::from_ref(&inner.addr).cast::<SOCKADDR>(),
                            inner.addr_len,
                        )
                    };
                    Answer::Done(from.map(|from| (n, Addr::Inet(from))))
                }
                Answer::Done(Err(err)) => Answer::Done(Err(err)),
                Answer::Undone { not_ready } => Answer::Undone { not_ready },
            };
            Outcome::RecvFrom(buf, answer)
        }
        PortMem::RecvLentFrom(mut buf) => {
            let answer = match answer {
                Answer::Done(Ok(n)) => {
                    // SAFETY: the kernel wrote the first `n` bytes of the
                    // spare capacity.
                    unsafe { buf.set_len(buf.len() + n) };
                    // SAFETY: the kernel wrote the sender's address and its
                    // length into the slot.
                    let from = unsafe {
                        sys::socket_addr(
                            core::ptr::from_ref(&inner.addr).cast::<SOCKADDR>(),
                            inner.addr_len,
                        )
                    };
                    Answer::Done(from.map(|from| (n, Addr::Inet(from))))
                }
                Answer::Done(Err(err)) => Answer::Done(Err(err)),
                Answer::Undone { not_ready } => Answer::Undone { not_ready },
            };
            Outcome::RecvLentFrom(buf, answer)
        }
        PortMem::NoAccept => Outcome::Accept(match answer {
            Answer::Done(Ok(_)) => Answer::Done(Err(io::ErrorKind::Other.into())),
            Answer::Done(Err(err)) => {
                crate::rt::counters::accept_error(&err);
                Answer::Done(Err(err))
            }
            Answer::Undone { not_ready } => Answer::Undone { not_ready },
        }),
        PortMem::Accept(conn) => Outcome::Accept(match answer {
            Answer::Done(Ok(_)) => Answer::Done(accepted_peer(inner).map(|peer| (conn, peer))),
            Answer::Done(Err(err)) => {
                crate::rt::counters::accept_error(&err);
                Answer::Done(Err(err))
            }
            Answer::Undone { not_ready } => Answer::Undone { not_ready },
        }),
        PortMem::Connect(dial) => Outcome::Connect(
            dial,
            match answer {
                Answer::Done(result) => Answer::Done(result.map(drop)),
                Answer::Undone { not_ready } => Answer::Undone { not_ready },
            },
        ),
        PortMem::Send(buf) => Outcome::Send(buf, answer),
        PortMem::SendLent(buf) => Outcome::SendLent(buf, answer),
        PortMem::SendTo(buf, target) => Outcome::SendTo(buf, target, answer),
        PortMem::SendLentTo(buf) => Outcome::SendLentTo(buf, answer),
        // The lent write's end, with no buffer to give back.
        PortMem::SendFile(_) => Outcome::SendLent(OwnedBuf::default(), answer),
    }
}

/// The peer of the connection `AcceptEx` left in the slot's address room.
fn accepted_peer(inner: &Inner) -> io::Result<Addr> {
    let ext = winsock::ext()?;
    let mut local: *mut SOCKADDR = core::ptr::null_mut();
    let mut remote: *mut SOCKADDR = core::ptr::null_mut();
    let (mut local_len, mut remote_len) = (0i32, 0i32);
    // SAFETY: the room is the one `AcceptEx` filled, with the same sizes;
    // the call points into it.
    unsafe {
        (ext.get_accept_ex_sockaddrs)(
            inner.accept.as_ptr().cast::<c_void>(),
            0,
            ACCEPT_ADDR as DWORD,
            ACCEPT_ADDR as DWORD,
            &mut local,
            &mut local_len,
            &mut remote,
            &mut remote_len,
        );
        sys::socket_addr(remote, remote_len).map(Addr::Inet)
    }
}

impl Source {
    /// The way this thread reaches the port the source is registered with,
    /// or `None` when it is not registered with one.
    pub(super) fn route<'a>(
        self: &'a Arc<Self>,
        _current: Option<&'a crate::rt::shard::Shard>,
    ) -> Option<PortIo<'a>> {
        if !self.reg.ops.load(Ordering::Acquire) {
            return None;
        }
        if self.reg.state.load(Ordering::Acquire) != super::REGISTERED {
            return Some(PortIo::NotYet);
        }
        Some(PortIo::Here { source: self })
    }

    /// Cancels the side's operation `id`; the caller holds that side. If
    /// the kernel took none of its bytes yet, it ends undone.
    pub(super) fn cancel_op(&self, id: u64) {
        let Some(slot) = self.port.slots.get(id as usize) else {
            return;
        };
        // SAFETY: a side is held, so the copy is not being written.
        if unsafe { (*self.port.sync.get()).is_some() } {
            slot.job.cancel();
            return;
        }
        if !self.may_cancel(id) {
            return;
        }
        let fd = self.fd.load(Ordering::Acquire);
        if fd == super::NO_FD {
            return;
        }
        // SAFETY: a cancel names the side's own OVERLAPPED on its own handle;
        // one that finds the operation done does nothing.
        unsafe { CancelIoEx(fd as usize as HANDLE, slot.ov.get()) };
    }

    /// Whether the side's operation `id` may be cancelled. A TCP send may
    /// not: a send the stack has taken bytes of can come back cancelled
    /// with none counted, and sending them again would send them twice. It
    /// is left to end on its own (as the bytes behind a handle are delivered
    /// whatever happens to it).
    fn may_cancel(&self, id: u64) -> bool {
        !(id == WRITE && self.kind == Kind::Tcp)
    }

    /// Registers the source with port shard `shard` under `token`, with
    /// both sides held: its socket's completions go to the shard's port
    /// from now on (moved from a port it had, whose shard went). A handle
    /// with operations still out cannot move: the registration then waits
    /// for them to end, and is made again by the tasks they wake (see
    /// [`waits_for_ops`](Self::waits_for_ops)).
    pub(super) fn attach_port(
        &self,
        shard: Arc<crate::rt::shard::Shared>,
        token: u64,
        fd: Raw,
    ) -> io::Result<()> {
        let bind = || match self.kind {
            // A handle opened for synchronous IO completes on no port: its
            // calls are the pool's, on a copy of it.
            Kind::Bytes => sys::synchronous(fd).and_then(|sync| match sync {
                true => sys::duplicate_file(fd).map(|copy| Some(Arc::new(copy))),
                false => shard.port().bind(fd as usize as HANDLE).map(|()| None),
            }),
            Kind::Udp => shard
                .port()
                .bind(fd as usize as HANDLE)
                .and_then(|()| sys::udp_connreset_off(fd))
                .map(|()| None),
            _ => shard.port().bind(fd as usize as HANDLE).map(|()| None),
        };
        let bound = match bind() {
            Err(_) if self.waits_for_ops() => {
                self.reg.state.store(super::UNREGISTERED, Ordering::Release);
                shard.release_source(token);
                return Ok(());
            }
            // None is out (any more): once more, and that answer stands.
            Err(_) => bind(),
            bound => bound,
        };
        let sync = match bound {
            Ok(sync) => sync,
            Err(err) => {
                shard.counters.refused_registration();
                self.reg.state.store(super::UNREGISTERED, Ordering::Release);
                shard.release_source(token);
                return Err(err);
            }
        };
        // SAFETY: both sides are held.
        unsafe { *self.port.sync.get() = sync };
        self.reg.ops.store(true, Ordering::Release);
        self.reg
            .home
            .store(Arc::as_ptr(&shard) as usize, Ordering::Release);
        self.reg.token.store(token, Ordering::Release);
        // SAFETY: both sides are held.
        unsafe { *self.reg.shard.get() = Some(shard) };
        self.reg.state.store(super::REGISTERED, Ordering::Release);
        Ok(())
    }

    /// The source's registration with a port goes: every operation it has
    /// out that may be cancelled is (its packet still comes, wherever the
    /// socket's completions go then); a TCP send goes on (see
    /// [`may_cancel`](Self::may_cancel)). Both sides are held.
    pub(super) fn detach_port(&self) {
        // SAFETY: both sides are held; the jobs keep their own copies.
        if unsafe { (*self.port.sync.get()).take() }.is_some() {
            for slot in &self.port.slots {
                slot.job.cancel();
            }
            return;
        }
        let fd = self.fd.load(Ordering::Acquire);
        if fd == super::NO_FD {
            return;
        }
        for id in [READ, WRITE] {
            if self.may_cancel(id) {
                // SAFETY: cancels the side's own operation on the source's
                // own handle; one done or none out is nothing to do.
                unsafe { CancelIoEx(fd as usize as HANDLE, self.port.slots[id as usize].ov.get()) };
            }
        }
    }
}
