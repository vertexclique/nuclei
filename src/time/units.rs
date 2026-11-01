//! Nanoseconds in whole units of a wheel (milliseconds, microseconds).

#[cfg(test)]
mod tests;

/// The millisecond wheel's units in a second, and the microsecond one's.
pub(crate) const MILLIS: u64 = 1_000;
pub(crate) const MICROS: u64 = 1_000_000;

/// `nanos` in units of `1 / per_second` (a divisor of a second's
/// nanoseconds), rounded up or down.
#[inline(always)]
pub(crate) fn units(nanos: u64, per_second: u64, up: bool) -> u64 {
    let nanos_per_unit = 1_000_000_000 / per_second;
    if up {
        nanos.div_ceil(nanos_per_unit)
    } else {
        nanos / nanos_per_unit
    }
}
