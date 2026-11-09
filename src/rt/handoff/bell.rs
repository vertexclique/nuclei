//! How the monitor sleeps and is woken: [`Bell`].
//!
//! A timed wait ends at its deadline, and a ring ends any wait at once. A
//! thread's timed park ends at its deadline on Linux and macOS, but on
//! Windows at the system timer's next tick (15.6 ms unless a process asked
//! for finer): the monitor's 2.5 ms tick would take 15.6 ms there, and a
//! 10 ms threshold would act as about 62 ms. On Windows the bell therefore
//! waits on an event and a high-resolution waitable timer; a system without
//! that timer (before Windows 10 1803) or a failed call parks instead. On
//! Apple's systems a timed park is a timer the kernel coalesces with others:
//! on a CI runner a 2.5 ms park took 18 ms at the median and up to 119 ms,
//! so the 10 ms threshold acted as 70 ms or more. The bell waits on a kqueue
//! there instead, a user event for the ring and a timer marked critical,
//! which is not coalesced, as a shard's precise sleep's is
//! (src/rt/park/kqueue.rs): 2.5 ms took 3.8 ms at the median and 8.6 ms at
//! most. A queue that cannot be made, or a failed call, parks.

use std::sync::OnceLock;
use std::thread::Thread;
use std::time::Duration;

/// The monitor's wait: for a ring, or for a ring or a deadline.
pub(super) struct Bell {
    thread: OnceLock<Thread>,
    #[cfg(windows)]
    handles: Option<windows::Handles>,
    #[cfg(target_vendor = "apple")]
    handles: Option<apple::Handles>,
    /// The rings so far, for a test to count.
    #[cfg(test)]
    rings: core::sync::atomic::AtomicUsize,
}

impl Bell {
    pub(super) fn new() -> Self {
        Self {
            thread: OnceLock::new(),
            #[cfg(windows)]
            handles: windows::Handles::new(),
            #[cfg(target_vendor = "apple")]
            handles: apple::Handles::new(),
            #[cfg(test)]
            rings: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The rings so far.
    #[cfg(test)]
    pub(super) fn rings(&self) -> usize {
        self.rings.load(core::sync::atomic::Ordering::SeqCst)
    }

    /// Makes the calling thread the one that waits on the bell.
    pub(super) fn bind(&self) {
        let _ = self.thread.set(std::thread::current());
    }

    /// Waits for a ring (or wakes early, spuriously).
    pub(super) fn wait(&self) {
        kovan::flush_pending();
        #[cfg(any(windows, target_vendor = "apple"))]
        if self.handles.as_ref().is_some_and(|h| h.wait(None)) {
            return;
        }
        std::thread::park();
    }

    /// Waits for a ring or for `left` to pass (or wakes early, spuriously).
    pub(super) fn wait_for(&self, left: Duration) {
        kovan::flush_pending();
        #[cfg(any(windows, target_vendor = "apple"))]
        if self.handles.as_ref().is_some_and(|h| h.wait(Some(left))) {
            return;
        }
        std::thread::park_timeout(left);
    }

    /// Ends the waiting thread's wait, or its next one.
    pub(super) fn ring(&self) {
        #[cfg(test)]
        self.rings
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        #[cfg(any(windows, target_vendor = "apple"))]
        if let Some(handles) = &self.handles {
            handles.ring();
        }
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::time::Duration;

    use crate::syscore::windows::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CloseHandle, CreateEventW, CreateWaitableTimerExW,
        HANDLE, INFINITE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WAIT_OBJECT_0,
        WaitForMultipleObjects, WaitForSingleObject,
    };

    /// An auto-reset event, which a ring sets, and a high-resolution timer.
    pub(super) struct Handles {
        event: HANDLE,
        timer: HANDLE,
    }

    // SAFETY: an event's and a timer's handles may be used from any thread.
    unsafe impl Send for Handles {}
    // SAFETY: as above.
    unsafe impl Sync for Handles {}

    impl Handles {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: plain arguments; the handles are owned at once.
            unsafe {
                let event = CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null());
                if event.is_null() {
                    return None;
                }
                let timer = CreateWaitableTimerExW(
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                );
                if timer.is_null() {
                    CloseHandle(event);
                    return None;
                }
                Some(Self { event, timer })
            }
        }

        /// Waits on the event, and with `left` on the timer set to it too;
        /// false when a call failed.
        pub(super) fn wait(&self, left: Option<Duration>) -> bool {
            // SAFETY: plain arguments on our own handles.
            unsafe {
                let Some(left) = left else {
                    return WaitForSingleObject(self.event, INFINITE) == WAIT_OBJECT_0;
                };
                // Relative due times are negative, in 100 ns units, rounded up.
                let due = -(i64::try_from(left.as_nanos().div_ceil(100)).unwrap_or(i64::MAX));
                if SetWaitableTimer(
                    self.timer,
                    &due,
                    0,
                    core::ptr::null(),
                    core::ptr::null_mut(),
                    0,
                ) == 0
                {
                    return false;
                }
                let both = [self.event, self.timer];
                WaitForMultipleObjects(2, both.as_ptr(), 0, INFINITE) < WAIT_OBJECT_0 + 2
            }
        }

        pub(super) fn ring(&self) {
            // SAFETY: our own event.
            unsafe {
                SetEvent(self.event);
            }
        }
    }

    impl Drop for Handles {
        fn drop(&mut self) {
            // SAFETY: our own handles, closed once.
            unsafe {
                CloseHandle(self.timer);
                CloseHandle(self.event);
            }
        }
    }
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    /// The ident of the ring's user event and of the wait's timer (their
    /// filters keep them apart).
    const IDENT: libc::uintptr_t = 0;

    /// A kqueue with a user event, which a ring triggers, and a timer marked
    /// critical, armed for each timed wait.
    pub(super) struct Handles {
        queue: OwnedFd,
    }

    /// A change of the queue's `filter`, built from a zeroed struct.
    fn change(filter: i16, flags: u16, fflags: u32, data: isize) -> libc::kevent {
        // SAFETY: kevent is plain data; all zeroes is a valid value.
        let mut event: libc::kevent = unsafe { core::mem::zeroed() };
        event.ident = IDENT;
        event.filter = filter;
        event.flags = flags;
        event.fflags = fflags;
        event.data = data;
        event
    }

    impl Handles {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: a plain call; the descriptor is owned at once.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return None;
            }
            // SAFETY: `raw` is a new descriptor nobody else owns.
            let queue = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: sets close-on-exec on our own descriptor.
            unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
            let handles = Self { queue };
            // EV_CLEAR: a ring is kept until a wait takes it, as a park's token.
            let user = change(libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, 0);
            handles.submit(&user, None).then_some(handles)
        }

        /// Submits `change` and, with `out`, waits for one event into it;
        /// false when the call failed.
        fn submit(&self, change: &libc::kevent, out: Option<&mut libc::kevent>) -> bool {
            let (events, n) = match out {
                Some(event) => (core::ptr::from_mut(event), 1),
                None => (core::ptr::null_mut(), 0),
            };
            // SAFETY: one change read from `change`, at most one event written
            // to `events`, no timeout: the wait ends on an event.
            let ret = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    change,
                    1,
                    events,
                    n,
                    core::ptr::null(),
                )
            };
            ret >= 0
        }

        /// Waits for a ring, and with `left` for its timer too; false when a
        /// call failed.
        pub(super) fn wait(&self, left: Option<Duration>) -> bool {
            // SAFETY: kevent is plain data; all zeroes is a valid value.
            let mut out: libc::kevent = unsafe { core::mem::zeroed() };
            let Some(left) = left else {
                // SAFETY: as in `submit`, with no change.
                let ret = unsafe {
                    libc::kevent(
                        self.queue.as_raw_fd(),
                        core::ptr::null(),
                        0,
                        &mut out,
                        1,
                        core::ptr::null(),
                    )
                };
                return ret >= 0;
            };
            let nanos = isize::try_from(left.as_nanos()).unwrap_or(isize::MAX);
            let timer = change(
                libc::EVFILT_TIMER,
                libc::EV_ADD | libc::EV_ONESHOT,
                libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
                nanos,
            );
            self.submit(&timer, Some(&mut out))
        }

        pub(super) fn ring(&self) {
            // A failure means the queue is gone, so nobody waits on it.
            let _ = self.submit(&change(libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0), None);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::time::{Duration, Instant};

    use super::Bell;

    /// A ring before the wait ends it at once, as a park's token does.
    #[test]
    fn a_ring_before_the_wait_ends_it_at_once() {
        let bell = Bell::new();
        bell.bind();
        bell.ring();
        let start = Instant::now();
        bell.wait_for(Duration::from_secs(20));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the ring was lost"
        );
    }

    /// A timed wait with no ring ends no sooner than its deadline.
    #[test]
    fn a_timed_wait_lasts_at_least_its_time() {
        let bell = Bell::new();
        bell.bind();
        for micros in [500, 2500, 10_000] {
            let left = Duration::from_micros(micros);
            let start = Instant::now();
            // A spurious early end is allowed; the loop that waits looks at
            // the clock, as the monitor's tick does.
            while start.elapsed() < left {
                bell.wait_for(left.saturating_sub(start.elapsed()));
            }
            assert!(start.elapsed() >= left);
        }
    }

    /// On Apple's systems the bell waits on its kqueue, whose timer is
    /// marked critical so the kernel does not coalesce it, not on a park: a
    /// 2.5 ms wait ends within a few milliseconds, where a timed park took
    /// 18 ms at the median on a CI runner. The median of many waits is
    /// judged, with room for a loaded machine.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_bell_times_its_waits_with_a_critical_timer_on_apple() {
        let bell = Bell::new();
        assert!(bell.handles.is_some(), "the bell parks");
        bell.bind();
        let left = Duration::from_micros(2500);
        let mut took: Vec<Duration> = (0..64)
            .map(|_| {
                let start = Instant::now();
                bell.wait_for(left);
                start.elapsed()
            })
            .collect();
        took.sort_unstable();
        assert!(
            took[took.len() / 2] < Duration::from_millis(10),
            "a 2.5 ms wait took {:?} at the median",
            took[took.len() / 2]
        );
    }
}
