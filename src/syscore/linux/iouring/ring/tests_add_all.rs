//! A buffer ring filled in one pass: the same descriptors as one add per id, and no pass over
//! descriptors queued and not committed.

use std::io::ErrorKind;

use super::tests::{buf_ring_or_skip, ring_or_skip};

#[test]
fn adding_every_buffer_at_once_writes_what_one_add_per_id_writes() {
    let Some(ring) = ring_or_skip() else { return };
    let (Some(mut one_by_one), Some(mut all)) = (
        buf_ring_or_skip(&ring, 1, 8, 256),
        buf_ring_or_skip(&ring, 2, 8, 256),
    ) else {
        return;
    };
    for bid in 0..8 {
        one_by_one.add(bid).unwrap();
    }
    one_by_one.commit();
    all.add_all().unwrap();
    all.commit();
    assert_eq!(all.published_tail(), one_by_one.published_tail());
    let base = |r: &super::buf_ring::BufRing| r.descriptor(0).addr;
    for slot in 0..8 {
        let (a, b) = (all.descriptor(slot), one_by_one.descriptor(slot));
        assert_eq!((a.len, a.bid), (b.len, b.bid), "descriptor {slot}");
        assert_eq!(
            a.addr - base(&all),
            b.addr - base(&one_by_one),
            "descriptor {slot}"
        );
        assert_eq!(a.bid, slot);
        assert_eq!(a.addr - base(&all), u64::from(slot) * 256);
    }
}

#[test]
fn adding_every_buffer_over_uncommitted_ones_queues_none() {
    let Some(ring) = ring_or_skip() else { return };
    let Some(mut pool) = buf_ring_or_skip(&ring, 3, 4, 64) else {
        return;
    };
    pool.add(0).unwrap();
    let err = pool.add_all().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    pool.commit();
    assert_eq!(pool.published_tail(), 1, "only the one add was queued");
}
