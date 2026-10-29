//! Quartz: a coarse monotonic clock for bounding how long a shard's run
//! loop goes without looking at its IO driver.
//!
//! A reading is nanoseconds from an arbitrary start, never going backwards.
//! It costs a few nanoseconds and never a system call: on Linux and Android
//! the kernel's coarse clock is read from the vDSO (`CLOCK_MONOTONIC_COARSE`,
//! which the kernel advances once per scheduler tick), on macOS the
//! approximate raw clock (`CLOCK_MONOTONIC_RAW_APPROX`), on Windows the tick
//! count (`GetTickCount64`), and elsewhere `std::time::Instant` from the
//! first reading.
//!
//! The resolution is coarse: 1 to 4 ms on Linux (one tick), about 16 ms on
//! Windows. A reading is therefore good for telling "a while has passed", never
//! for a timer, a deadline or a measurement: the timers and the runtime's
//! clock are `time::clock`.

/// Reads the coarse monotonic clock.
pub(crate) struct Quartz;

impl Quartz {
    /// The clock now, in nanoseconds from an arbitrary start.
    #[inline]
    pub(crate) fn now() -> u64 {
        read()
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
#[inline]
fn read() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC_COARSE;
    #[cfg(target_os = "macos")]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW_APPROX;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to fill in.
    let rc = unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    if rc != 0 {
        // A clock the platform refuses reads as standing still, so a caller
        // that bounds by it drives at its other cadence only.
        return 0;
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

#[cfg(windows)]
#[inline]
fn read() -> u64 {
    // SAFETY: takes nothing and touches no memory.
    unsafe { crate::syscore::windows::GetTickCount64() }.wrapping_mul(1_000_000)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn read() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::Quartz;
    use std::time::{Duration, Instant};

    #[test]
    fn it_never_goes_backwards_and_advances() {
        let first = Quartz::now();
        let mut last = first;
        // Longer than the coarsest tick the platforms give (Windows: about 16 ms).
        let deadline = Instant::now() + Duration::from_secs(5);
        while last.wrapping_sub(first) < 30_000_000 {
            assert!(Instant::now() < deadline, "the clock never advanced");
            let now = Quartz::now();
            assert!(now >= last, "the clock went backwards");
            last = now;
            std::thread::yield_now();
        }
    }
}
