//! A shard is written in place, in its `Arc` or its `Rc`: every field as its constructor says, its
//! two wheels empty and working, and a shard that cannot be made writes nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Limits, Shard};
use crate::rt::context;
use crate::rt::tuning::{QUARTZ_EVERY, Tuning};

fn limits() -> Limits {
    Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    }
}

#[test]
fn a_workers_shard_is_written_in_its_arc_with_its_place_and_tuning() {
    let tuning = Tuning {
        inbox_interval: 7,
        driver_interval: 5,
        lifo: false,
        ..Tuning::default()
    };
    let shard = Shard::worker(limits(), tuning, 3, 2, None).unwrap();
    assert_eq!(Arc::strong_count(&shard), 1);
    assert_eq!(shard.owner.get(), Some((3, 2)));
    assert!(shard.locals().is_some(), "a worker's shard has its locals");
    assert_eq!(shard.until_inbox.get(), 7);
    assert!(!shard.tuning.lifo);
    assert_eq!(shard.drive_at.get(), 5.min(QUARTZ_EVERY));
    assert_eq!(shard.drive_left.get(), shard.drive_at.get());
    assert!(shard.monitor.is_none());
    assert_eq!(shard.wheel().borrow().armed(), 0);
    assert_eq!(shard.precise.borrow().armed(), 0);
}

#[test]
fn a_threads_own_shard_is_written_in_its_rc_and_fires_both_wheels() {
    let shard = Shard::new_rc(limits()).unwrap();
    assert_eq!(std::rc::Rc::strong_count(&shard), 1);
    assert_eq!(shard.owner.get(), None);
    assert!(shard.locals().is_none(), "only a worker's shard has locals");
    assert_eq!(shard.until_inbox.get(), Tuning::default().inbox_interval);
    assert_eq!((shard.drive_at.get(), shard.drive_left.get()), (1, 1));
    let _current = context::enter(&shard);
    let start = Instant::now();
    shard.block_on(async {
        crate::time::sleep(Duration::from_millis(2)).await;
        crate::time::sleep_precise(Duration::from_micros(300)).await;
    });
    assert!(start.elapsed() >= Duration::from_millis(2));
    assert_eq!(shard.wheel().borrow().armed(), 0);
    assert_eq!(shard.precise.borrow().armed(), 0);
}

/// A ring's receive group that leaves the pool no room for a buffer of its own is refused
/// before the driver is made, and the shard after it is made whole.
#[cfg(ring)]
#[test]
fn a_shard_refused_writes_nothing_and_the_next_is_made() {
    use crate::rt::backend::Backend;
    use crate::syscore::linux::iouring::ring::RingConfig;
    let refused = Limits {
        max_nesting: 8,
        buffer_cap: 1,
        backend: Backend::Ring(RingConfig {
            recv_buffers: 4,
            recv_buffer_size: 4096,
            ..RingConfig::new(64)
        }),
    };
    assert!(Shard::new_rc(refused).is_err());
    assert!(Shard::worker(refused, Tuning::default(), 0, 0, None).is_err());
    let shard = Shard::new_rc(limits()).unwrap();
    let _current = context::enter(&shard);
    shard.block_on(crate::time::sleep(Duration::from_millis(1)));
}
