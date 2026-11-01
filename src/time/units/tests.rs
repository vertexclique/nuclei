use std::time::Duration;

use proptest::prelude::*;

use super::{MICROS, MILLIS, units};

/// What the wheels computed before: the nanoseconds of a span divided on 128
/// bits.
fn on_128_bits(nanos: u64, per_second: u64, up: bool) -> u64 {
    let unit = u128::from(1_000_000_000 / per_second);
    let n = Duration::from_nanos(nanos).as_nanos();
    let q = if up { n.div_ceil(unit) } else { n / unit };
    u64::try_from(q).unwrap_or(u64::MAX)
}

proptest! {
    #[test]
    fn units_match_the_division_of_the_nanoseconds(
        nanos in any::<u64>(),
        micro in any::<bool>(),
        up in any::<bool>(),
    ) {
        let per_second = if micro { MICROS } else { MILLIS };
        prop_assert_eq!(units(nanos, per_second, up), on_128_bits(nanos, per_second, up));
    }
}

#[test]
fn a_part_of_a_unit_rounds_as_asked() {
    assert_eq!(units(2_000_000_001, MILLIS, true), 2_001);
    assert_eq!(units(2_000_000_001, MILLIS, false), 2_000);
    assert_eq!(units(999, MICROS, true), 1);
    assert_eq!(units(0, MICROS, true), 0);
    assert_eq!(units(u64::MAX, MICROS, true), u64::MAX / 1_000 + 1);
    assert_eq!(units(u64::MAX, MILLIS, false), u64::MAX / 1_000_000);
}
