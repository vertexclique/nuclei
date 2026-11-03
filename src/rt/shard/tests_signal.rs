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

/// A `block_on` future on a watched shard that finishes in the very poll whose end finds the
/// shard taken: `block_on` answers its output, the thread is detached, and the waker the call
/// would have kept for the next one stays out of the shard another thread now runs
/// (proofs/tla/handoff, `M_ready_first`).
#[test]
fn a_future_done_in_the_poll_that_found_the_take_keeps_no_waker_on_the_shard() {
    let shard = shard();
    let slot = crate::rt::handoff::slot::Slot::new(false);
    let _current = context::enter_watched(&shard, Some(&slot));
    let output = shard.block_on(async {
        slot.claim();
        assert!(slot.take(), "the poll was not taken");
        7
    });
    assert_eq!(output, 7);
    assert!(
        context::current(|current| current.is_none()),
        "the thread still runs the taken shard"
    );
    assert!(
        shard.signal.take().is_none(),
        "the block_on kept its waker in the taken shard's cell"
    );
}
