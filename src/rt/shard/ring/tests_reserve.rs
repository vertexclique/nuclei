//! A shard's receive group: one the system has no memory for fails the
//! shard's start.

use std::io;

use crate::rt::backend::Backend;
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
