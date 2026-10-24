//! `cancelled` and `cancelled_owned`: the future completes on cancel, and the owned form may
//! be moved while pending.

use super::*;

#[test]
fn cancel_token() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    let wait_fut = token.cancelled();
    let mut wait_fut = pin!(wait_fut);
    assert_eq!(probe.enter(|cx| wait_fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(probe.wakes(), 0);

    let wait_fut_2 = token.cancelled();
    let mut wait_fut_2 = pin!(wait_fut_2);

    token.cancel();
    assert_eq!(probe.wakes(), 1);
    assert!(token.is_cancelled());

    assert_eq!(
        probe.enter(|cx| wait_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| wait_fut_2.as_mut().poll(cx)),
        Poll::Ready(())
    );
}

#[test]
fn cancel_token_owned() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    let wait_fut = token.clone().cancelled_owned();
    let mut wait_fut = pin!(wait_fut);
    assert_eq!(probe.enter(|cx| wait_fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(probe.wakes(), 0);

    let wait_fut_2 = token.clone().cancelled_owned();
    let mut wait_fut_2 = pin!(wait_fut_2);

    token.cancel();
    assert_eq!(probe.wakes(), 1);
    assert!(token.is_cancelled());

    assert_eq!(
        probe.enter(|cx| wait_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| wait_fut_2.as_mut().poll(cx)),
        Poll::Ready(())
    );
}

#[test]
fn cancel_token_owned_drop_test() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();

    let future = token.cancelled_owned();
    let mut future = pin!(future);
    assert_eq!(probe.enter(|cx| future.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(probe.wakes(), 0);
    // Dropped while pinned and pending, with the token it owns.
}

#[test]
fn the_owned_future_may_be_moved_while_pending() {
    let token = CancellationToken::new();
    let mut task = Task::new(token.clone().cancelled_owned());
    task.pending();
    let moved = Box::new(Task::new(token.clone().cancelled_owned()));
    let mut moved = *moved;
    moved.pending();
    token.cancel();
    assert_eq!((task.wakes(), moved.wakes()), (1, 1));
    task.ready();
    moved.ready();
}
