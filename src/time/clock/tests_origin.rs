//! The origin's reading of the monotonic clock: known where std's instants are that clock's
//! readings, and the same as what std's own arithmetic gives.

use std::time::{Duration, Instant};

use super::{monotonic, nanos, reading_of, system_nanos};

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn an_instant_reads_as_the_monotonic_clock_between_two_readings() {
    let before = monotonic().unwrap();
    let at = Instant::now();
    let after = monotonic().unwrap();
    let reading = reading_of(at).expect("std's instants are the monotonic clock's readings here");
    assert!(
        before <= reading && reading <= after,
        "{before} <= {reading} <= {after}"
    );
    let earlier = at.checked_sub(Duration::from_millis(3)).unwrap();
    assert_eq!(
        reading_of(earlier),
        Some(reading - 3_000_000),
        "an earlier instant reads as the later one's reading less their difference"
    );
}

#[test]
fn the_clock_read_directly_agrees_with_std() {
    let low = nanos(Instant::now());
    let direct = system_nanos();
    let high = nanos(Instant::now());
    assert!(
        low <= direct && direct <= high,
        "{low} <= {direct} <= {high}"
    );
}
