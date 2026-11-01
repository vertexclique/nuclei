//! A span of time in whole units of a wheel (milliseconds, microseconds),
//! on 64 bits.

use std::time::Duration;

#[cfg(test)]
mod tests;

/// The millisecond wheel's units in a second, and the microsecond one's.
pub(crate) const MILLIS: u64 = 1_000;
pub(crate) const MICROS: u64 = 1_000_000;

/// `span` in units of `1 / per_second` (a divisor of a second's
/// nanoseconds), rounded up or down, `u64::MAX` past its range: what
/// dividing `Duration::as_nanos` gives, as a whole second is a whole number
/// of units, without its 128-bit division.
#[inline(always)]
pub(crate) fn units(span: Duration, per_second: u64, up: bool) -> u64 {
    let nanos_per_unit = 1_000_000_000 / per_second;
    let part = u64::from(span.subsec_nanos());
    let part = if up {
        part.div_ceil(nanos_per_unit)
    } else {
        part / nanos_per_unit
    };
    span.as_secs()
        .checked_mul(per_second)
        .and_then(|whole| whole.checked_add(part))
        .unwrap_or(u64::MAX)
}
