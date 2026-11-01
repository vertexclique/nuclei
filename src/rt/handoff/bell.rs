//! How the monitor sleeps and is woken: [`Bell`].
//!
//! A timed wait ends at its deadline, and a ring ends any wait at once. A
//! thread's timed park ends at its deadline on Linux and macOS, but on
//! Windows at the system timer's next tick (15.6 ms unless a process asked
//! for finer): the monitor's 2.5 ms tick would take 15.6 ms there, and a
//! 10 ms threshold would act as about 62 ms. On Windows the bell therefore
//! waits on an event and a high-resolution waitable timer; a system without
//! that timer (before Windows 10 1803) or a failed call parks instead.

use std::sync::OnceLock;
use std::thread::Thread;
use std::time::Duration;

/// The monitor's wait: for a ring, or for a ring or a deadline.
pub(super) struct Bell {
    thread: OnceLock<Thread>,
    #[cfg(windows)]
    handles: Option<windows::Handles>,
}

impl Bell {
    pub(super) fn new() -> Self {
        Self {
            thread: OnceLock::new(),
            #[cfg(windows)]
            handles: windows::Handles::new(),
        }
    }

    /// Makes the calling thread the one that waits on the bell.
    pub(super) fn bind(&self) {
        let _ = self.thread.set(std::thread::current());
    }

    /// Waits for a ring (or wakes early, spuriously).
    pub(super) fn wait(&self) {
        #[cfg(windows)]
        if self.handles.as_ref().is_some_and(|h| h.wait(None)) {
            return;
        }
        std::thread::park();
    }

    /// Waits for a ring or for `left` to pass (or wakes early, spuriously).
    pub(super) fn wait_for(&self, left: Duration) {
        #[cfg(windows)]
        if self.handles.as_ref().is_some_and(|h| h.wait(Some(left))) {
            return;
        }
        std::thread::park_timeout(left);
    }

    /// Ends the waiting thread's wait, or its next one.
    pub(super) fn ring(&self) {
        #[cfg(windows)]
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
