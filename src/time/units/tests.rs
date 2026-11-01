use std::time::Duration;

use proptest::prelude::*;

use super::{MICROS, MILLIS, units};

/// What the wheels computed before: the nanoseconds divided on 128 bits.
fn on_128_bits(span: Duration, per_second: u64, up: bool) -> u64 {
    let unit = u128::from(1_000_000_000 / per_second);
    let n = span.as_nanos();
    let q = if up { n.div_ceil(unit) } else { n / unit };
    u64::try_from(q).unwrap_or(u64::MAX)
}

proptest! {
    #[test]
    fn units_match_the_division_of_the_nanoseconds(
        secs in any::<u64>(),
        nanos in 0u32..1_000_000_000,
        micro in any::<bool>(),
        up in any::<bool>(),
    ) {
        let span = Duration::new(secs, nanos);
        let per_second = if micro { MICROS } else { MILLIS };
        prop_assert_eq!(units(span, per_second, up), on_128_bits(span, per_second, up));
    }

    #[test]
    fn units_match_at_the_ranges_edge(
        secs in (u64::MAX / MICROS - 2)..=(u64::MAX / MILLIS + 2),
        nanos in 0u32..1_000_000_000,
        micro in any::<bool>(),
        up in any::<bool>(),
    ) {
        let span = Duration::new(secs, nanos);
        let per_second = if micro { MICROS } else { MILLIS };
        prop_assert_eq!(units(span, per_second, up), on_128_bits(span, per_second, up));
    }
}

#[test]
fn a_part_of_a_unit_rounds_as_asked() {
    let span = Duration::new(2, 1);
    assert_eq!(units(span, MILLIS, true), 2_001);
    assert_eq!(units(span, MILLIS, false), 2_000);
    assert_eq!(units(Duration::new(0, 999), MICROS, true), 1);
    assert_eq!(units(Duration::ZERO, MICROS, true), 0);
}
