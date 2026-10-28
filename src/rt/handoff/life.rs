//! A runtime thread as the monitor and a shutdown see it: whether its shard
//! was taken from it while it was blocked (a shutdown does not wait for such
//! a thread), and its id, to let it off its core.

use crate::sync::internal::{AtomicBool, AtomicU64, Ordering};

/// A worker or spare thread of a runtime.
pub(crate) struct Life {
    /// Its shard was taken while it was in one poll; it has not come back.
    detached: AtomicBool,
    /// The OS id of the thread (Linux, Windows), 0 until it starts.
    id: AtomicU64,
}

impl Life {
    pub(crate) fn new() -> Self {
        Self {
            detached: AtomicBool::new(false),
            id: AtomicU64::new(0),
        }
    }

    /// Records the calling thread as this one, as it starts.
    pub(crate) fn here(&self) {
        self.id.store(os_thread_id(), Ordering::Release);
    }

    /// The thread's shard was taken from it (the monitor).
    pub(crate) fn detach(&self) {
        self.detached.store(true, Ordering::Release);
    }

    /// The thread came back from the poll it was blocked in.
    pub(crate) fn back(&self) {
        self.detached.store(false, Ordering::Release);
    }

    /// Whether the thread is out on a blocked poll, its shard taken.
    pub(crate) fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    /// Lets the thread run on any of `cores` (the process's own): a blocked
    /// thread gives its core to the shard's new runner.
    pub(crate) fn unpin(&self, cores: &[usize]) {
        let id = self.id.load(Ordering::Acquire);
        if id != 0 && !cores.is_empty() {
            unpin(id, cores);
        }
    }
}

#[cfg(target_os = "linux")]
fn os_thread_id() -> u64 {
    // SAFETY: no arguments; it cannot fail.
    u64::try_from(unsafe { libc::gettid() }).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn unpin(id: u64, cores: &[usize]) {
    let Ok(tid) = libc::pid_t::try_from(id) else {
        return;
    };
    // SAFETY: a zeroed set of our own, filled below, and a thread id of this
    // process (a gone thread's id fails the call, which is harmless).
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        for &core in cores {
            libc::CPU_SET(core, &mut set);
        }
        let _ = libc::sched_setaffinity(tid, size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(windows)]
fn os_thread_id() -> u64 {
    // SAFETY: no arguments; it cannot fail.
    u64::from(unsafe { crate::syscore::windows::GetCurrentThreadId() })
}

#[cfg(windows)]
fn unpin(id: u64, cores: &[usize]) {
    use crate::syscore::windows::{CloseHandle, OpenThread, SetThreadAffinityMask};
    /// THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION.
    const ACCESS: u32 = 0x0020 | 0x0040;
    let Ok(id) = u32::try_from(id) else {
        return;
    };
    let mask = cores
        .iter()
        .filter(|&&core| core < usize::BITS as usize)
        .fold(0usize, |mask, &core| mask | (1 << core));
    // SAFETY: a thread id of this process; the handle is closed below.
    unsafe {
        let thread = OpenThread(ACCESS, 0, id);
        if !thread.is_null() {
            SetThreadAffinityMask(thread, mask);
            CloseHandle(thread);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn os_thread_id() -> u64 {
    0
}

#[cfg(not(any(target_os = "linux", windows)))]
fn unpin(_id: u64, _cores: &[usize]) {}
