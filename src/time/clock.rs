//! A runtime's clock: the time [`Instant::now`](super::Instant::now), the
//! timers and the timer wheels read. It runs with the system's monotonic
//! clock until it is paused; paused, it stands still until it is advanced,
//! by hand or by itself when the runtime has nothing to do.
//!
//! Times are nanoseconds after one process-wide origin. A clock that was never
//! paused or advanced reads the system clock as it is, and while no clock of
//! any live runtime was, [`now`] is one relaxed load more than
//! `std::time::Instant::now()`. A paused or advanced clock keeps a high-water
//! mark of every time it gave, so its reads never go backwards, on any
//! thread, across a pause, a resume or an advance.

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Clocks of live runtimes that were paused or advanced: while none is, every
/// clock reads the system's.
static ALTERED: AtomicUsize = AtomicUsize::new(0);

/// The origin every clock's nanoseconds count from, and every shard's timer
/// wheel its milliseconds: an instant of std's, and the reading of the
/// system's monotonic clock it is, where that clock is read directly.
struct Origin {
    at: Instant,
    reading: Option<u64>,
}

fn origin_of() -> &'static Origin {
    static ORIGIN: OnceLock<Origin> = OnceLock::new();
    ORIGIN.get_or_init(|| {
        let at = Instant::now();
        Origin {
            at,
            reading: reading_of(at),
        }
    })
}

/// The origin every clock's nanoseconds count from, and every shard's timer
/// wheel its milliseconds.
pub(crate) fn origin() -> Instant {
    origin_of().at
}

/// Nanoseconds from the origin to `at`; zero before it.
pub(crate) fn nanos(at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(origin()).as_nanos()).unwrap_or(u64::MAX)
}

/// The instant `nanos` after the origin.
pub(crate) fn instant(nanos: u64) -> Instant {
    origin() + Duration::from_nanos(nanos)
}

/// Whether the clock is read directly, not through std's instants.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
pub(crate) fn reads_directly() -> bool {
    origin_of().reading.is_some()
}

/// The origin's reading of the monotonic clock, kept where one load reaches
/// it: [`UNREAD`] until the origin is made, [`THROUGH_STD`] where the clock
/// is read through std's instants.
static READING: AtomicU64 = AtomicU64::new(UNREAD);
const UNREAD: u64 = u64::MAX;
const THROUGH_STD: u64 = u64::MAX - 1;

/// The system's monotonic clock now, in nanoseconds after the origin: what
/// [`nanos`] of `Instant::now()` gives, read without std's arithmetic where
/// std's instants are the clock's readings (Linux, Android), and through it
/// elsewhere.
#[inline]
pub(crate) fn system_nanos() -> u64 {
    let origin = READING.load(Ordering::Relaxed);
    if origin < THROUGH_STD
        && let Some(now) = monotonic()
    {
        return now.saturating_sub(origin);
    }
    system_nanos_slow(origin)
}

/// [`system_nanos`] before the origin is read, or through std.
#[cold]
#[inline(never)]
fn system_nanos_slow(origin: u64) -> u64 {
    if origin == UNREAD {
        let reading = origin_of().reading.unwrap_or(THROUGH_STD);
        READING.store(reading, Ordering::Relaxed);
        if reading < THROUGH_STD
            && let Some(now) = monotonic()
        {
            return now.saturating_sub(reading);
        }
    }
    nanos(Instant::now())
}

/// The monotonic clock std's instants read, in nanoseconds: on Linux and
/// Android, `CLOCK_MONOTONIC`.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[inline]
fn monotonic() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to fill in.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    // The monotonic clock counts up from boot: neither field is negative, and
    // the nanoseconds fit 64 bits for centuries.
    Some(
        (ts.tv_sec as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec as u64),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[inline]
fn monotonic() -> Option<u64> {
    None
}

/// The monotonic clock's reading that std's instant `at` is, where std's
/// instants are that clock's readings and say so: read from the seconds and
/// nanoseconds its `Debug` form shows, once, then checked against the clock
/// itself (an instant taken between two readings lies between them, its
/// reading being `at`'s plus std's own exact difference of the two). `None`
/// where any of that does not hold: every reading then goes through std.
fn reading_of(at: Instant) -> Option<u64> {
    let shown = format!("{at:?}");
    let field = |name: &str| -> Option<u64> {
        let from = shown.find(name)? + name.len();
        let digits = shown[from..].trim_start_matches(' ');
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        digits[..end].parse().ok()
    };
    let reading = field("tv_sec:")?
        .checked_mul(1_000_000_000)?
        .checked_add(field("tv_nsec:")?)?;
    let before = monotonic()?;
    let probe = Instant::now();
    let after = monotonic()?;
    let since = u64::try_from(probe.checked_duration_since(at)?.as_nanos()).ok()?;
    let probe = reading.checked_add(since)?;
    (before <= probe && probe <= after).then_some(reading)
}

/// A runtime's clock.
pub(crate) struct Clock {
    /// The clock stands still.
    paused: AtomicBool,
    /// Paused: its time. Running: unused.
    base: AtomicU64,
    /// Running: nanoseconds added to the system's time (negative after a
    /// pause, positive after an advance while running).
    skew: AtomicI64,
    /// The latest time read, so reads never go backwards.
    last: AtomicU64,
    /// Counted in [`ALTERED`].
    altered: AtomicBool,
    /// An advance of the paused clock is being decided
    /// ([`try_advance`](crate::rt::paused::try_advance)).
    advancing: AtomicBool,
    /// Bumped each time a shard joins or leaves the runtime's guests.
    members: AtomicU64,
}

impl Clock {
    pub(crate) fn new(start_paused: bool) -> Self {
        let clock = Self {
            paused: AtomicBool::new(false),
            base: AtomicU64::new(0),
            skew: AtomicI64::new(0),
            last: AtomicU64::new(0),
            altered: AtomicBool::new(false),
            advancing: AtomicBool::new(false),
            members: AtomicU64::new(0),
        };
        if start_paused {
            clock.pause();
        }
        clock
    }

    /// The clock's time now.
    pub(crate) fn now(&self) -> Instant {
        self.now_from(Instant::now())
    }

    /// The clock's time now, `system` being the system's time read before
    /// the call. Read before the look at the mark: a reading that finds the
    /// clock unaltered took it before a pause marked it, and so before the
    /// time that pause stops at (`pause`).
    fn now_from(&self, system: Instant) -> Instant {
        if !self.altered.load(Ordering::Acquire) {
            return system;
        }
        instant(self.now_nanos_from(|| nanos(system)))
    }

    pub(crate) fn now_nanos(&self) -> u64 {
        self.now_nanos_from(system_nanos)
    }

    /// [`now_nanos`](Self::now_nanos), the system's time from `system`, asked
    /// only while the clock runs.
    fn now_nanos_from(&self, system: impl FnOnce() -> u64) -> u64 {
        let read = if self.paused.load(Ordering::Acquire) {
            self.base.load(Ordering::Acquire)
        } else {
            system().saturating_add_signed(self.skew.load(Ordering::Acquire))
        };
        let last = self.last.fetch_max(read, Ordering::AcqRel);
        read.max(last)
    }

    /// Marks the clock altered, once.
    fn alter(&self) {
        if !self.altered.swap(true, Ordering::AcqRel) {
            ALTERED.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[inline]
    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Whether the clock was ever paused or advanced.
    #[inline]
    pub(crate) fn is_altered(&self) -> bool {
        self.altered.load(Ordering::Acquire)
    }

    /// Claims the decision of an advance; false while another holds it.
    pub(crate) fn claim_advance(&self) -> bool {
        self.advancing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn release_advance(&self) {
        self.advancing.store(false, Ordering::SeqCst);
    }

    /// Waits until no advance is being decided: a shard that just woke reads
    /// the clock only after a jump in progress is done.
    pub(crate) fn wait_advance(&self) {
        while self.advancing.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
    }

    pub(crate) fn members(&self) -> u64 {
        self.members.load(Ordering::SeqCst)
    }

    pub(crate) fn member_changed(&self) {
        self.members.fetch_add(1, Ordering::SeqCst);
    }

    /// Stops the clock where it is; a paused clock stays as it is.
    pub(crate) fn pause(&self) {
        // Marked before the time it stops at is read, a fence between: a
        // reading that found the clock unmarked left no trace in `last`, and
        // took the system's time before it looked (`now`), so before this
        // mark and before the time read here (proofs/tla/clock/ClockRead.tla,
        // `M_pause_reads_first`).
        self.alter();
        core::sync::atomic::fence(Ordering::SeqCst);
        let now = self.now_nanos();
        #[cfg(test)]
        tests::after_pause_reads();
        if self.paused.load(Ordering::Acquire) {
            return;
        }
        // On the wheels' millisecond grid (a step of less than a millisecond
        // forward), so a timer armed for a whole number of milliseconds from
        // the time the clock stands at fires at that time exactly.
        let now = now.div_ceil(1_000_000).saturating_mul(1_000_000);
        self.base.store(now, Ordering::Release);
        self.paused.store(true, Ordering::Release);
        // A read that ran between the two stores may be later than `now`.
        self.base
            .fetch_max(self.last.load(Ordering::Acquire), Ordering::AcqRel);
        self.last.fetch_max(now, Ordering::AcqRel);
    }

    /// Starts a paused clock again from where it stands; a running clock
    /// stays as it is.
    pub(crate) fn resume(&self) {
        if !self.paused.load(Ordering::Acquire) {
            return;
        }
        let base = self.base.load(Ordering::Acquire);
        let skew = i128::from(base) - i128::from(nanos(Instant::now()));
        let skew = i64::try_from(skew).unwrap_or(if skew < 0 { i64::MIN } else { i64::MAX });
        self.skew.store(skew, Ordering::Release);
        self.paused.store(false, Ordering::Release);
    }

    /// Moves the clock forward by `by`, paused or running.
    pub(crate) fn advance(&self, by: Duration) {
        self.alter();
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        if self.paused.load(Ordering::Acquire) {
            let _ = self
                .base
                .try_update(Ordering::AcqRel, Ordering::Acquire, |b| {
                    Some(b.saturating_add(by))
                });
        } else {
            let by = i64::try_from(by).unwrap_or(i64::MAX);
            let _ = self
                .skew
                .try_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                    Some(s.saturating_add(by))
                });
        }
    }

    /// Moves a paused clock forward to `at` (nanoseconds after the origin),
    /// if it is behind it.
    pub(crate) fn advance_to_nanos(&self, at: u64) {
        self.base.fetch_max(at, Ordering::AcqRel);
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        if *self.altered.get_mut() {
            ALTERED.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Whether any live runtime's clock was paused or advanced: while none was,
/// every clock is the system's and the park path skips the paused clock's
/// work.
#[inline]
pub(crate) fn any_altered() -> bool {
    ALTERED.load(Ordering::Relaxed) != 0
}

/// Nanoseconds after the clock origin of `at`, for the deadlines shards
/// publish.
pub(crate) fn to_nanos(at: Instant) -> u64 {
    nanos(at)
}

/// [`now`] in nanoseconds after the origin: what the timers and the wheels
/// read.
///
/// The system's time is read before the look at [`ALTERED`], as in
/// [`Clock::now`]: a reading that finds no clock altered took it before any
/// pause marked its clock.
#[inline]
pub(crate) fn now_nanos() -> u64 {
    let system = system_nanos();
    if ALTERED.load(Ordering::Relaxed) == 0 {
        return system;
    }
    crate::rt::runtime::with_clock(|clock| {
        clock.map_or(system, |clock| clock.now_nanos_from(|| system))
    })
}

/// The time now on the clock of the runtime this thread is inside (or of the
/// default runtime, if one runs), or the system's; read as [`now_nanos`].
#[inline]
pub(crate) fn now() -> Instant {
    let system = Instant::now();
    if ALTERED.load(Ordering::Relaxed) == 0 {
        return system;
    }
    crate::rt::runtime::with_clock(|clock| clock.map_or(system, |clock| clock.now_from(system)))
}

#[cfg(test)]
mod tests {
    use super::*;

    std::thread_local! {
        /// What a test runs in a pause right after it read the time it
        /// stops at, once.
        static AFTER_PAUSE_READS: core::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { core::cell::RefCell::new(None) };
    }

    pub(super) fn after_pause_reads() {
        if let Some(hook) = AFTER_PAUSE_READS.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
    }

    #[test]
    fn a_reading_taken_as_a_pause_begins_is_never_gone_back_on() {
        let clock = std::sync::Arc::new(Clock::new(false));
        let seen = std::sync::Arc::new(AtomicU64::new(0));
        let (reader, read) = (std::sync::Arc::clone(&clock), std::sync::Arc::clone(&seen));
        // A reading of the clock between the time the pause read and its
        // first store: later than that time by more than the millisecond
        // the pause rounds up to.
        AFTER_PAUSE_READS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::thread::sleep(Duration::from_millis(3));
                read.store(to_nanos(reader.now()), Ordering::SeqCst);
            }));
        });
        clock.pause();
        let before = seen.load(Ordering::SeqCst);
        assert_ne!(before, 0, "the reading ran inside the pause");
        assert!(
            to_nanos(clock.now()) >= before,
            "the paused clock reads earlier than a reading it gave before"
        );
    }

    #[test]
    fn a_paused_clock_stands_still_until_advanced() {
        let clock = Clock::new(true);
        let a = clock.now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clock.now(), a);
        clock.advance(Duration::from_secs(3));
        assert_eq!(clock.now(), a + Duration::from_secs(3));
        clock.advance_to_nanos(to_nanos(a + Duration::from_secs(2)));
        assert_eq!(clock.now(), a + Duration::from_secs(3), "never back");
        clock.advance_to_nanos(to_nanos(a + Duration::from_secs(5)));
        assert_eq!(clock.now(), a + Duration::from_secs(5));
    }

    #[test]
    fn a_resumed_clock_runs_on_from_where_it_stood() {
        let clock = Clock::new(true);
        let stood = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        clock.resume();
        let after = clock.now();
        assert!(after >= stood);
        assert!(
            after - stood < Duration::from_millis(20),
            "the time paused is not counted"
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(clock.now() >= after + Duration::from_millis(5));
    }

    #[test]
    fn reads_never_go_backwards_across_pauses_on_many_threads() {
        let clock = std::sync::Arc::new(Clock::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let clock = std::sync::Arc::clone(&clock);
                std::thread::spawn(move || {
                    let mut last = clock.now();
                    for _ in 0..20_000 {
                        let now = clock.now();
                        assert!(now >= last, "the clock went back");
                        last = now;
                    }
                })
            })
            .collect();
        for i in 0..200 {
            if i % 2 == 0 {
                clock.pause();
                clock.advance(Duration::from_micros(3));
            } else {
                clock.resume();
            }
        }
        for reader in readers {
            reader.join().unwrap();
        }
    }

    #[test]
    fn an_advance_while_running_moves_the_clock_ahead() {
        let clock = Clock::new(false);
        let before = clock.now();
        clock.advance(Duration::from_secs(60));
        assert!(clock.now() >= before + Duration::from_secs(60));
        assert!(!clock.is_paused());
    }
}

#[cfg(test)]
mod tests_origin;
