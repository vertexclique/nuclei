//! The monitor's heavy fence: one call that orders every running thread of
//! the process, so a runner's way back into its shard needs only the
//! compiler's fence.
//!
//! On return from [`heavy`], every store a thread of the process made
//! before the call is visible to the caller, and every load a thread makes
//! after it sees the caller's stores from before the call: Linux's
//! `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)` (4.14, after a one-time
//! registration), Windows' `FlushProcessWriteBuffers`. Elsewhere (macOS,
//! the BSDs, a Linux that refuses the call) there is none and the runner's
//! side takes a full fence instead ([`super::slot::Slot`]).

/// Whether this process has a heavy fence. Asked before any worker of a
/// runtime runs, whose slots then take the compiler's fence only; on Linux
/// it registers the process for the fence the first time.
pub(crate) fn available() -> bool {
    imp::available()
}

/// The heavy fence. `false` when the call failed, which [`available`] said
/// it would not: the caller must not rely on it then.
pub(crate) fn heavy() -> bool {
    imp::heavy()
}

#[cfg(all(target_os = "linux", not(miri), not(all(loom, test))))]
mod imp {
    use std::sync::OnceLock;

    fn membarrier(cmd: libc::c_int) -> bool {
        // SAFETY: the call reads no memory of the caller's; flags and CPU
        // id are zero.
        unsafe { libc::syscall(libc::SYS_membarrier, cmd, 0, 0) == 0 }
    }

    pub(super) fn available() -> bool {
        static REGISTERED: OnceLock<bool> = OnceLock::new();
        *REGISTERED.get_or_init(|| membarrier(libc::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED))
    }

    pub(super) fn heavy() -> bool {
        membarrier(libc::MEMBARRIER_CMD_PRIVATE_EXPEDITED)
    }
}

#[cfg(all(windows, not(miri), not(all(loom, test))))]
mod imp {
    pub(super) fn available() -> bool {
        true
    }

    pub(super) fn heavy() -> bool {
        // SAFETY: no arguments; the call cannot fail.
        unsafe { crate::syscore::windows::FlushProcessWriteBuffers() };
        true
    }
}

#[cfg(any(not(any(target_os = "linux", windows)), miri, all(loom, test)))]
mod imp {
    pub(super) fn available() -> bool {
        false
    }

    pub(super) fn heavy() -> bool {
        crate::sync::internal::fence(crate::sync::internal::Ordering::SeqCst);
        true
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #[test]
    fn a_heavy_fence_where_there_is_one_succeeds() {
        if super::available() {
            assert!(super::heavy());
            assert!(super::heavy(), "again");
        }
    }
}
