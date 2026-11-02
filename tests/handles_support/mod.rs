//! The descriptors (on Windows the handles) this process holds open, as the
//! tests that count what a process holds read them: one count for all of
//! them, so each sees a leak the same way.

/// From the directory the kernel keeps for the process.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn open_handles() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// FreeBSD lists only the first three in `/dev/fd` without `fdescfs`: its
/// kernel counts them (`kern.proc.nfds`).
#[cfg(target_os = "freebsd")]
pub fn open_handles() -> usize {
    let name = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_NFDS, 0];
    let mut count: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>();
    // SAFETY: the kernel writes one int, this process's count, into our own.
    let rc = unsafe {
        libc::sysctl(
            name.as_ptr(),
            name.len() as libc::c_uint,
            (&raw mut count).cast(),
            &mut len,
            core::ptr::null(),
            0,
        )
    };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    count as usize
}

/// From `/dev/fd`, which macOS and the other BSDs list whole.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "freebsd"))
))]
pub fn open_handles() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[cfg(windows)]
pub fn open_handles() -> usize {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    let mut count = 0u32;
    // SAFETY: the pseudo handle of this process, and a place for the count.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
    count as usize
}
