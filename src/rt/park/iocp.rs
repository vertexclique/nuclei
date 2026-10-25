//! The Windows poller: one I/O completion port per shard, the packets of
//! the shard's sources' overlapped operations, wakes posted from any thread,
//! and a high-resolution waitable timer for the shard's timed waits.
//!
//! A port wait counts milliseconds on the system's 15.6 ms tick, too coarse
//! for the timer wheel; so a timed wait arms a high-resolution waitable
//! timer and associates it with the port through a wait completion packet,
//! and waits on the port alone. A system without those waits on the port's
//! own timeout, rounded up so it never ends early.
//!
//! The port counts the operations issued to it that the kernel has not
//! completed: a shard that stops running hands a port with operations still
//! out to a keeper on the blocking pool ([`Port::keep`]), which delivers
//! their packets until none is out or the shard runs again; so no packet of
//! an operation whose memory the kernel holds is lost, and none waits for a
//! shard that is not running (a TCP send is never cancelled, so it may stay
//! out for as long as its peer reads nothing).

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::syscore::windows::{
    BOOLEAN, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CloseHandle,
    CreateIoCompletionPort, CreateWaitableTimerExW, FILE_COMPLETION_INFORMATION,
    FILE_REPLACE_COMPLETION_INFORMATION, FILE_SKIP_SET_EVENT_ON_HANDLE, GENERIC_ALL,
    GetQueuedCompletionStatusEx, HANDLE, INFINITE, IO_STATUS_BLOCK,
    NtAssociateWaitCompletionPacket, NtCancelWaitCompletionPacket, NtCreateWaitCompletionPacket,
    NtSetInformationFile, OVERLAPPED_ENTRY, PostQueuedCompletionStatus, STATUS_SUCCESS,
    SetFileCompletionNotificationModes, SetWaitableTimer, TIMER_ALL_ACCESS, ULONG, WAIT_TIMEOUT,
    last_error,
};

/// The completion key of a wake; sources' operations never use it.
const WAKE: usize = usize::MAX;
/// The completion key of the timer's packet.
const TIMER: usize = usize::MAX - 1;
/// The completion key of the sources' operations, which the `OVERLAPPED`
/// of each packet names.
pub(crate) const SOURCE: usize = 0;
/// The completion key of a blocking job's packet: an operation on a handle
/// opened for synchronous IO, whose call a thread of the pool made.
pub(crate) const JOB: usize = usize::MAX - 2;

/// A shard's port, shared with the threads that issue operations to it and
/// wake it.
pub(crate) struct Port {
    handle: HANDLE,
    /// Operations issued to this port whose packets were not dequeued yet.
    out: AtomicUsize,
}

// SAFETY: a port handle is used from any thread by design of the API.
unsafe impl Send for Port {}
// SAFETY: as for `Send`.
unsafe impl Sync for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the port is ours; nothing uses it once the last `Arc` goes.
        unsafe { CloseHandle(self.handle) };
    }
}

impl Port {
    /// The port's handle, which lives as long as this.
    pub(crate) fn handle(&self) -> HANDLE {
        self.handle
    }

    /// An operation is issued with this port as its destination.
    pub(crate) fn issued(&self) {
        self.out.fetch_add(1, Ordering::AcqRel);
    }

    /// The packet of an operation issued to this port was dequeued, on this
    /// port or on another the handle was moved to.
    pub(crate) fn completed(&self) {
        self.out.fetch_sub(1, Ordering::AcqRel);
    }

    /// Operations issued to this port and not completed.
    pub(crate) fn out(&self) -> usize {
        self.out.load(Ordering::Acquire)
    }

    /// Delivers the packets of the port's operations with `deliver` until
    /// none is out or `stopped` says so, for a port whose shard does not
    /// run; a wake (which is what ends the wait for a stop) is nobody's then.
    pub(crate) fn keep(
        &self,
        stopped: impl Fn() -> bool,
        mut deliver: impl FnMut(&OVERLAPPED_ENTRY),
    ) {
        let mut entries = [OVERLAPPED_ENTRY::zeroed(); 64];
        while self.out() > 0 && !stopped() {
            let mut removed: ULONG = 0;
            // SAFETY: the entries buffer has room for its length in packets;
            // the kernel says how many it wrote.
            let ok = unsafe {
                GetQueuedCompletionStatusEx(
                    self.handle,
                    entries.as_mut_ptr(),
                    entries.len() as ULONG,
                    &mut removed,
                    INFINITE,
                    0,
                )
            };
            // A wait with no timeout on a port this borrow keeps open fails
            // for no reason it could go on from.
            if ok == 0 {
                return;
            }
            for entry in &entries[..removed as usize] {
                if !matches!(entry.lpCompletionKey, WAKE | TIMER) && !entry.lpOverlapped.is_null() {
                    deliver(entry);
                }
            }
        }
    }

    /// Moves the completions of `handle` to this port: at once if it is not
    /// bound to one yet, else from the one it is bound to (whose shard
    /// went). A completion of an operation already out still reaches its
    /// source, wherever it lands.
    pub(crate) fn bind(&self, handle: HANDLE) -> io::Result<()> {
        // SAFETY: plain arguments; `handle` is open for the call.
        let bound = unsafe { CreateIoCompletionPort(handle, self.handle, SOURCE, 0) };
        if bound.is_null() {
            let first = last_error();
            let mut status = IO_STATUS_BLOCK {
                Status: 0,
                Information: 0,
            };
            let mut info = FILE_COMPLETION_INFORMATION {
                Port: self.handle,
                Key: SOURCE as *mut c_void,
            };
            // SAFETY: the information block is a FILE_COMPLETION_INFORMATION,
            // as the class takes it.
            let moved = unsafe {
                NtSetInformationFile(
                    handle,
                    &mut status,
                    core::ptr::from_mut(&mut info).cast(),
                    size_of::<FILE_COMPLETION_INFORMATION>() as ULONG,
                    FILE_REPLACE_COMPLETION_INFORMATION,
                )
            };
            if moved != STATUS_SUCCESS {
                return Err(first);
            }
        }
        // SAFETY: plain arguments. A failure leaves the event set on each
        // completion, which costs a little and changes nothing else.
        unsafe { SetFileCompletionNotificationModes(handle, FILE_SKIP_SET_EVENT_ON_HANDLE) };
        Ok(())
    }
}

/// A high-resolution waitable timer and the wait packet that brings its
/// firing to the port.
struct Timer {
    timer: HANDLE,
    packet: HANDLE,
    /// The packet is associated with the timer and may still be queued.
    armed: bool,
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: both handles are ours.
        unsafe {
            CloseHandle(self.packet);
            CloseHandle(self.timer);
        }
    }
}

/// A shard's poller on its port.
pub(crate) struct Poller {
    port: Arc<Port>,
    timer: Option<Timer>,
    entries: Vec<OVERLAPPED_ENTRY>,
}

// SAFETY: the timer's handles are used by the one thread that owns the
// poller; the port is `Sync`.
unsafe impl Send for Poller {}

/// Wakes a poller from any thread.
#[derive(Clone)]
pub(crate) struct Notifier {
    port: Arc<Port>,
}

impl Poller {
    pub(crate) fn new() -> io::Result<Self> {
        crate::syscore::windows::winsock::start()?;
        // SAFETY: a new port. Its shard drains it, or a keeper while the
        // shard does not run; no limit on the threads its packets release,
        // since a thread that drained a port counts against the limit while
        // it runs (the shard after it handed the port on, a pool thread after
        // it kept one) and would hold the next drainer's packets back.
        let handle = unsafe {
            CreateIoCompletionPort(
                crate::syscore::windows::INVALID_HANDLE_VALUE,
                core::ptr::null_mut(),
                0,
                ULONG::MAX,
            )
        };
        if handle.is_null() {
            return Err(last_error());
        }
        Ok(Self {
            port: Arc::new(Port {
                handle,
                out: AtomicUsize::new(0),
            }),
            timer: Timer::new(),
            entries: vec![OVERLAPPED_ENTRY::zeroed(); 256],
        })
    }

    pub(crate) fn notifier(&self) -> Notifier {
        Notifier {
            port: Arc::clone(&self.port),
        }
    }

    pub(crate) fn port(&self) -> &Arc<Port> {
        &self.port
    }

    /// Blocks until a notifier fires, a packet comes, or `timeout` passes
    /// (`None` waits, a zero timeout only looks), taking at most `max`
    /// packets (at least one). Calls `on_packet` with each operation's
    /// packet. Returns whether a wake came.
    pub(crate) fn wait_at_most(
        &mut self,
        max: usize,
        timeout: Option<Duration>,
        mut on_packet: impl FnMut(&OVERLAPPED_ENTRY),
    ) -> io::Result<bool> {
        let ms = match timeout {
            None => INFINITE,
            Some(t) if t.is_zero() => 0,
            Some(t) => match self.timer.as_mut().map(|timer| timer.arm(&self.port, t)) {
                Some(Ok(())) => INFINITE,
                // No timer: the port's own timeout, rounded up.
                _ => t
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min(u128::from(INFINITE - 1)) as u32,
            },
        };
        let capacity = self.entries.len().min(max).max(1) as ULONG;
        let mut removed: ULONG = 0;
        // SAFETY: the entries buffer has room for `capacity` packets; the
        // kernel says how many it wrote.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.port.handle,
                self.entries.as_mut_ptr(),
                capacity,
                &mut removed,
                ms,
                0,
            )
        };
        if let Some(timer) = self.timer.as_mut() {
            timer.disarm();
        }
        if ok == 0 {
            let err = last_error();
            return match err.raw_os_error() {
                Some(WAIT_TIMEOUT) => Ok(false),
                _ => Err(err),
            };
        }
        let mut woken = false;
        for entry in &self.entries[..removed as usize] {
            match entry.lpCompletionKey {
                WAKE => woken = true,
                TIMER => {
                    if let Some(timer) = self.timer.as_mut() {
                        timer.armed = false;
                    }
                }
                _ if entry.lpOverlapped.is_null() => {}
                _ => on_packet(entry),
            }
        }
        Ok(woken)
    }
}

impl Timer {
    /// A high-resolution timer and its packet, or `None` where the system
    /// has none (before Windows 10 1803).
    fn new() -> Option<Self> {
        // SAFETY: plain arguments; the handles are owned at once.
        unsafe {
            let timer = CreateWaitableTimerExW(
                core::ptr::null_mut(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            );
            if timer.is_null() {
                return None;
            }
            let mut packet: HANDLE = core::ptr::null_mut();
            if NtCreateWaitCompletionPacket(&mut packet, GENERIC_ALL, core::ptr::null_mut())
                != STATUS_SUCCESS
            {
                CloseHandle(timer);
                return None;
            }
            Some(Self {
                timer,
                packet,
                armed: false,
            })
        }
    }

    /// Sets the timer to fire after `after` and brings its firing to `port`.
    fn arm(&mut self, port: &Port, after: Duration) -> io::Result<()> {
        // Relative due times are negative, in 100 ns units, rounded up.
        let due = -(i64::try_from(after.as_nanos().div_ceil(100)).unwrap_or(i64::MAX));
        // SAFETY: plain arguments on our own handles.
        unsafe {
            if SetWaitableTimer(
                self.timer,
                &due,
                0,
                core::ptr::null(),
                core::ptr::null_mut(),
                0,
            ) == 0
            {
                return Err(last_error());
            }
            let mut already: BOOLEAN = 0;
            let status = NtAssociateWaitCompletionPacket(
                self.packet,
                port.handle,
                self.timer,
                TIMER as *mut c_void,
                core::ptr::null_mut(),
                STATUS_SUCCESS,
                0,
                &mut already,
            );
            if status != STATUS_SUCCESS {
                CancelWaitableTimer(self.timer);
                return Err(io::Error::from_raw_os_error(
                    crate::syscore::windows::status_error(status).unwrap_or(0),
                ));
            }
        }
        self.armed = true;
        Ok(())
    }

    /// Stops the timer, and takes its packet back if it was not dequeued.
    fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: our own handles; removing a packet already queued keeps
        // it from ending a later wait.
        unsafe {
            NtCancelWaitCompletionPacket(self.packet, 1);
            CancelWaitableTimer(self.timer);
        }
        self.armed = false;
    }
}

impl Notifier {
    pub(crate) fn notify(&self) {
        // SAFETY: a packet with no operation, on a port that lives as long
        // as this notifier. A full queue only drops a wake while another
        // is pending, which wakes the shard all the same.
        unsafe { PostQueuedCompletionStatus(self.port.handle, 0, WAKE, core::ptr::null_mut()) };
    }
}

#[cfg(test)]
mod tests;
