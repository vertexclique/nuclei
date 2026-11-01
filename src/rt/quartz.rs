//! Quartz: a monotonic clock for bounding how long a shard's run loop goes
//! without looking at its IO driver.
//!
//! A reading is nanoseconds from an arbitrary start, never going backwards,
//! in steps no longer than [`DRIVE_BOUND`], so the bound holds on every
//! platform. It costs a few nanoseconds and never a system call:
//! - Linux and Android: the kernel's coarse clock (`CLOCK_MONOTONIC_COARSE`,
//!   advanced once per scheduler tick) where its tick is no longer than the
//!   bound (a 1000 Hz kernel), else `CLOCK_MONOTONIC`; both from the vDSO, the
//!   choice made once from `clock_getres`;
//! - macOS: `CLOCK_UPTIME_RAW` (the `_APPROX` clocks may read a value
//!   milliseconds old);
//! - elsewhere, Windows included, `std::time::Instant` from the first reading
//!   (on Windows the performance counter; its tick count steps by 15.6 ms).
//!
//! A reading is for telling "a while has passed", never for a timer, a
//! deadline or a measurement: the timers and the runtime's clock are
//! `time::clock`.

/// Reads the drive bound's clock.
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
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to fill in.
    let rc = unsafe { libc::clock_gettime(clock(), &mut ts) };
    if rc != 0 {
        // A clock the platform refuses reads as standing still, so a caller
        // that bounds by it drives at its other cadence only.
        return 0;
    }
    nanos(&ts)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn nanos(ts: &libc::timespec) -> u64 {
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

/// The coarse clock where it steps by no more than the bound, else the
/// precise one, chosen at the first reading.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[inline]
fn clock() -> libc::clockid_t {
    use core::sync::atomic::{AtomicI32, Ordering};
    static CLOCK: AtomicI32 = AtomicI32::new(-1);
    let clock = CLOCK.load(Ordering::Relaxed);
    if clock >= 0 {
        return clock;
    }
    let mut tick = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `tick` is a valid timespec for the call to fill in.
    let known = unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC_COARSE, &mut tick) } == 0;
    let chosen = if known && nanos(&tick) <= super::tuning::DRIVE_BOUND {
        libc::CLOCK_MONOTONIC_COARSE
    } else {
        libc::CLOCK_MONOTONIC
    };
    CLOCK.store(chosen, Ordering::Relaxed);
    chosen
}

#[cfg(target_os = "macos")]
#[inline]
fn clock() -> libc::clockid_t {
    libc::CLOCK_UPTIME_RAW
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
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
    use crate::rt::tuning::DRIVE_BOUND;
    use std::time::{Duration, Instant};

    #[test]
    fn it_steps_by_no_more_than_the_drive_bound() {
        // The smallest change between readings is one step of the clock;
        // NTP may stretch a coarse tick a little.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut last = Quartz::now();
        let mut step = u64::MAX;
        while Instant::now() < deadline {
            let now = Quartz::now();
            if now != last {
                step = step.min(now - last);
                last = now;
            }
        }
        assert!(
            step <= DRIVE_BOUND + DRIVE_BOUND / 10,
            "the clock steps by {step} ns"
        );
    }

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
