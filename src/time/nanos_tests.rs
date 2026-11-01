//! The timers' nanosecond clock: the same time as std's instants, read
//! without their arithmetic where the system's clock allows it, and no
//! change to what a sleep or a deadline is.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::time::Duration;

use super::clock::{instant, nanos, system_nanos};
use super::{Instant, sleep, sleep_until};

#[test]
fn the_nanosecond_clock_reads_what_std_reads() {
    for _ in 0..1000 {
        let before = nanos(std::time::Instant::now());
        let now = system_nanos();
        let after = nanos(std::time::Instant::now());
        assert!(
            before <= now && now <= after,
            "{before} <= {now} <= {after}"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn on_linux_the_clock_is_read_directly() {
    assert!(
        super::clock::reads_directly(),
        "std's instants were not found to be the monotonic clock's readings"
    );
}

#[test]
fn an_instant_and_its_nanoseconds_convert_both_ways() {
    let at = std::time::Instant::now() + Duration::from_nanos(123_456_789);
    assert_eq!(instant(nanos(at)), at);
}

#[test]
fn a_sleep_from_a_duration_ends_at_now_plus_that_duration() {
    let d = Duration::from_millis(250);
    let start = Instant::now();
    let s = sleep(d);
    let end = Instant::now();
    assert!(start + d <= s.deadline() && s.deadline() <= end + d);
}

#[test]
fn a_sleep_past_the_clocks_range_ends_as_far_off_as_before() {
    let start = Instant::now();
    let s = sleep(Duration::MAX);
    assert!(s.deadline() >= start + Duration::from_secs(86_400 * 365 * 29));
}

#[test]
fn an_expired_deadline_is_ready_at_its_first_poll() {
    let mut cx = Context::from_waker(Waker::noop());
    crate::block_on(async {
        let mut past = sleep_until(Instant::now() - Duration::from_millis(5));
        assert_eq!(Pin::new(&mut past).poll(&mut cx), Poll::Ready(()));
        let mut now = sleep(Duration::ZERO);
        assert_eq!(Pin::new(&mut now).poll(&mut cx), Poll::Ready(()));
        let given = Instant::now() - Duration::from_secs(1);
        assert_eq!(sleep_until(given).deadline(), given);
    });
}
