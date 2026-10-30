//! A shard's receive group is charged to its IO buffer ledger: the pool has
//! the cap less the group, a cap that leaves it no buffer is refused, and a
//! group the system has no memory for fails the shard's start.

use std::io;

use crate::rt::backend::Backend;
use crate::rt::shard::recv::reservation;
use crate::rt::shard::{Limits, Shard};
use crate::syscore::linux::iouring::ring::RingConfig;
use crate::syscore::linux::iouring::ring::buf_ring::NO_MEMORY_FOR_TEST;

const BUFFERS: u16 = 4;
const SIZE: u32 = 4096;

fn ring(buffers: u16) -> RingConfig {
    RingConfig {
        recv_buffers: buffers,
        recv_buffer_size: SIZE,
        ..RingConfig::new(64)
    }
}

fn shard(backend: Backend, cap: usize) -> io::Result<Shard> {
    Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: cap,
        backend,
    })
}

/// Whether the kernel gives a ring with a receive group here.
fn rings() -> bool {
    match shard(Backend::Ring(ring(BUFFERS)), usize::MAX) {
        Ok(shard) if shard.ring().is_some_and(|r| r.recv_group().is_some()) => true,
        other => {
            eprintln!("skipping: no receive group here: {:?}", other.err());
            false
        }
    }
}

#[test]
fn the_pool_has_the_cap_less_the_group_and_one_byte_less_is_refused() {
    if !rings() {
        return;
    }
    let group = reservation(BUFFERS, SIZE);
    assert!(group >= usize::from(BUFFERS) * SIZE as usize + 16 * usize::from(BUFFERS));
    let least = group + crate::buf::CLASSES[0];
    let shard = shard(Backend::Ring(ring(BUFFERS)), least).expect("the least cap starts");
    let _entered = crate::rt::context::enter(&shard);
    let pool = shard.pool();
    assert_eq!((pool.reserved(), pool.charged()), (group, group));
    assert_eq!(shard.ring().unwrap().recv_reservation(), group);
    let one = pool.get(1).expect("one buffer beside the group");
    assert_eq!(one.capacity(), crate::buf::CLASSES[0]);
    assert!(pool.get(1).is_none(), "the cap is reached");
    drop(one);
    for backend in [
        Backend::Ring(ring(BUFFERS)),
        Backend::RingOrPoll(ring(BUFFERS)),
    ] {
        let Err(err) = self::shard(backend, least - 1) else {
            panic!("a shard started with no room beside its group");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        let message = err.to_string();
        for named in [
            "recv_buffers",
            "recv_buffer_size",
            "io_buffer_cap",
            &format!("{BUFFERS} x {SIZE}"),
            &format!("{group} bytes"),
            &format!("({} bytes)", least - 1),
        ] {
            assert!(message.contains(named), "{message} names no {named}");
        }
    }
}

#[test]
fn a_group_cut_to_a_small_ring_is_charged_as_made() {
    if !rings() {
        return;
    }
    // 64 entries are 128 completion slots: the group is cut to 64 buffers,
    // and the charge for the 128 asked for is given back down to them.
    let shard = shard(Backend::Ring(ring(128)), crate::buf::DEFAULT_CAP).unwrap();
    assert_eq!(shard.ring().unwrap().group_buffers(), Some(64));
    assert_eq!(shard.pool().reserved(), reservation(64, SIZE));
    assert_eq!(shard.pool().charged(), reservation(64, SIZE));
}

#[test]
fn the_poller_charges_no_group() {
    let shard = shard(Backend::Poll, crate::buf::CLASSES[0]).unwrap();
    assert_eq!(shard.pool().reserved(), 0);
}

#[test]
fn a_ring_that_goes_gives_its_group_back() {
    let Ok(ring) = super::ShardRing::new(&ring(BUFFERS), 0) else {
        eprintln!("skipping: io_uring unavailable");
        return;
    };
    let Some(_) = ring.recv_group() else {
        eprintln!("skipping: no receive group here");
        return;
    };
    assert_eq!(ring.shut_down(), reservation(BUFFERS, SIZE));
    assert_eq!(ring.recv_group(), None, "the group went with the call");
    assert_eq!(ring.recv_reservation(), 0);
}

#[test]
fn a_group_the_system_has_no_memory_for_fails_the_start_under_auto_too() {
    if !rings() {
        return;
    }
    NO_MEMORY_FOR_TEST.with(|t| t.set(true));
    let made = [
        Backend::Ring(ring(BUFFERS)),
        Backend::RingOrPoll(ring(BUFFERS)),
    ]
    .map(|backend| shard(backend, crate::buf::DEFAULT_CAP));
    NO_MEMORY_FOR_TEST.with(|t| t.set(false));
    for made in made {
        let Err(err) = made else {
            panic!("a shard started without the group it asked for");
        };
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory, "{err}");
        assert_eq!(err.raw_os_error(), None, "no shortage of the ring's own");
        let message = err.to_string();
        for named in ["recv_buffers", "recv_buffer_size", &format!("{SIZE} bytes")] {
            assert!(message.contains(named), "{message} names no {named}");
        }
    }
}
