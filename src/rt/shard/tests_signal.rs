use core::future::poll_fn;
use core::task::{Poll, Waker};

use super::{Limits, Shard};
use crate::rt::context;

fn shard() -> Shard {
    Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap()
}

fn waker_of_a_block_on(shard: &Shard) -> Waker {
    shard.block_on(poll_fn(|cx| Poll::Ready(cx.waker().clone())))
}

#[test]
fn a_block_on_reuses_the_waker_nothing_holds_any_more() {
    let shard = shard();
    let _current = context::enter(&shard);
    let first = waker_of_a_block_on(&shard).data();
    let second = waker_of_a_block_on(&shard).data();
    assert_eq!(first, second);
}

#[test]
fn a_waker_still_held_is_not_reused_until_it_goes() {
    let shard = shard();
    let _current = context::enter(&shard);
    let held = waker_of_a_block_on(&shard);
    let next = shard.block_on(poll_fn(|cx| Poll::Ready(cx.waker().data())));
    assert_ne!(next, held.data(), "a held waker was handed to a new future");
    held.wake();
    let after = shard.block_on(poll_fn(|cx| Poll::Ready(cx.waker().data())));
    assert_eq!(after, next, "the waker nothing holds was not reused");
}
