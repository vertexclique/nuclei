//! Child tokens: a cancel travels from the parent down, never up, through dropped
//! intermediates, and a child made after the cancel is born cancelled.

use super::*;

#[test]
fn cancel_child_token_through_parent() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();

    let child_token = token.child_token();
    assert!(!child_token.is_cancelled());

    let child_fut = child_token.cancelled();
    let mut child_fut = pin!(child_fut);
    let parent_fut = token.cancelled();
    let mut parent_fut = pin!(parent_fut);

    assert_eq!(probe.enter(|cx| child_fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(probe.wakes(), 0);

    token.cancel();
    assert_eq!(probe.wakes(), 2);
    assert!(token.is_cancelled());
    assert!(child_token.is_cancelled());

    assert_eq!(
        probe.enter(|cx| child_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
}

#[test]
fn cancel_grandchild_token_through_parent_if_child_was_dropped() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();

    let intermediate_token = token.child_token();
    let child_token = intermediate_token.child_token();
    drop(intermediate_token);
    assert!(!child_token.is_cancelled());

    let child_fut = child_token.cancelled();
    let mut child_fut = pin!(child_fut);
    let parent_fut = token.cancelled();
    let mut parent_fut = pin!(parent_fut);

    assert_eq!(probe.enter(|cx| child_fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(probe.wakes(), 0);

    token.cancel();
    assert_eq!(probe.wakes(), 2);
    assert!(token.is_cancelled());
    assert!(child_token.is_cancelled());

    assert_eq!(
        probe.enter(|cx| child_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
}

#[test]
fn cancel_child_token_without_parent() {
    let mut probe = Probe::new();
    let token = CancellationToken::new();

    let child_token_1 = token.child_token();
    let child_fut = child_token_1.cancelled();
    let mut child_fut = pin!(child_fut);
    let parent_fut = token.cancelled();
    let mut parent_fut = pin!(parent_fut);

    assert_eq!(probe.enter(|cx| child_fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(probe.wakes(), 0);

    child_token_1.cancel();
    assert_eq!(probe.wakes(), 1);
    assert!(!token.is_cancelled());
    assert!(child_token_1.is_cancelled());

    assert_eq!(
        probe.enter(|cx| child_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );

    let child_token_2 = token.child_token();
    let child_fut_2 = child_token_2.cancelled();
    let mut child_fut_2 = pin!(child_fut_2);

    assert_eq!(
        probe.enter(|cx| child_fut_2.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );

    token.cancel();
    assert_eq!(probe.wakes(), 3);
    assert!(token.is_cancelled());
    assert!(child_token_2.is_cancelled());

    assert_eq!(
        probe.enter(|cx| child_fut_2.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
}

#[test]
fn create_child_token_after_parent_was_cancelled() {
    for drop_child_first in [true, false] {
        let mut probe = Probe::new();
        let token = CancellationToken::new();
        token.cancel();

        let child_token = token.child_token();
        assert!(child_token.is_cancelled());

        {
            let child_fut = child_token.cancelled();
            let mut child_fut = pin!(child_fut);
            let parent_fut = token.cancelled();
            let mut parent_fut = pin!(parent_fut);

            assert_eq!(
                probe.enter(|cx| child_fut.as_mut().poll(cx)),
                Poll::Ready(())
            );
            assert_eq!(
                probe.enter(|cx| parent_fut.as_mut().poll(cx)),
                Poll::Ready(())
            );
            assert_eq!(probe.wakes(), 0);
        }

        if drop_child_first {
            drop(child_token);
            drop(token);
        } else {
            drop(token);
            drop(child_token);
        }
    }
}

#[test]
fn drop_multiple_child_tokens() {
    for drop_first_child_first in [true, false] {
        let token = CancellationToken::new();
        let mut child_tokens = [None, None, None];
        for child in &mut child_tokens {
            *child = Some(token.child_token());
        }

        assert!(!token.is_cancelled());
        assert!(!child_tokens[0].as_ref().unwrap().is_cancelled());

        for i in 0..child_tokens.len() {
            if drop_first_child_first {
                child_tokens[i] = None;
            } else {
                let last = child_tokens.len() - 1 - i;
                child_tokens[last] = None;
            }
            assert!(!token.is_cancelled());
        }

        drop(token);
    }
}

#[test]
fn cancel_only_all_descendants() {
    let mut probe = Probe::new();

    let parent_token = CancellationToken::new();
    let token = parent_token.child_token();
    let sibling_token = parent_token.child_token();
    let child1_token = token.child_token();
    let child2_token = token.child_token();
    let grandchild_token = child1_token.child_token();
    let grandchild2_token = child1_token.child_token();
    let great_grandchild_token = grandchild_token.child_token();

    for t in [
        &parent_token,
        &token,
        &sibling_token,
        &child1_token,
        &child2_token,
        &grandchild_token,
        &grandchild2_token,
        &great_grandchild_token,
    ] {
        assert!(!t.is_cancelled());
    }

    let parent_fut = parent_token.cancelled();
    let fut = token.cancelled();
    let sibling_fut = sibling_token.cancelled();
    let child1_fut = child1_token.cancelled();
    let child2_fut = child2_token.cancelled();
    let grandchild_fut = grandchild_token.cancelled();
    let grandchild2_fut = grandchild2_token.cancelled();
    let great_grandchild_fut = great_grandchild_token.cancelled();
    let mut parent_fut = pin!(parent_fut);
    let mut fut = pin!(fut);
    let mut sibling_fut = pin!(sibling_fut);
    let mut child1_fut = pin!(child1_fut);
    let mut child2_fut = pin!(child2_fut);
    let mut grandchild_fut = pin!(grandchild_fut);
    let mut grandchild2_fut = pin!(grandchild2_fut);
    let mut great_grandchild_fut = pin!(great_grandchild_fut);

    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Pending);
    assert_eq!(
        probe.enter(|cx| sibling_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| child1_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| child2_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| grandchild_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| grandchild2_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| great_grandchild_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(probe.wakes(), 0);

    token.cancel();

    assert_eq!(probe.wakes(), 6, "every waiter below and at `token`, once");
    assert!(!parent_token.is_cancelled());
    assert!(token.is_cancelled());
    assert!(!sibling_token.is_cancelled());
    assert!(child1_token.is_cancelled());
    assert!(child2_token.is_cancelled());
    assert!(grandchild_token.is_cancelled());
    assert!(grandchild2_token.is_cancelled());
    assert!(great_grandchild_token.is_cancelled());

    assert_eq!(probe.enter(|cx| fut.as_mut().poll(cx)), Poll::Ready(()));
    assert_eq!(
        probe.enter(|cx| child1_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| child2_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| grandchild_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| grandchild2_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(
        probe.enter(|cx| great_grandchild_fut.as_mut().poll(cx)),
        Poll::Ready(())
    );
    assert_eq!(probe.wakes(), 6);
    assert_eq!(
        probe.enter(|cx| parent_fut.as_mut().poll(cx)),
        Poll::Pending
    );
    assert_eq!(
        probe.enter(|cx| sibling_fut.as_mut().poll(cx)),
        Poll::Pending
    );
}

#[test]
fn drop_parent_before_child_tokens() {
    let token = CancellationToken::new();
    let child1 = token.child_token();
    let child2 = token.child_token();

    drop(token);
    assert!(!child1.is_cancelled());

    drop(child1);
    drop(child2);
}

#[test]
fn a_parent_without_handles_still_relays_a_grandparents_cancel() {
    let grandparent = CancellationToken::new();
    let parent = grandparent.child_token();
    let child = parent.child_token();
    let grandchild = child.child_token();
    drop(parent);
    drop(child);
    assert!(!grandchild.is_cancelled());
    grandparent.cancel();
    assert!(grandchild.is_cancelled());
}
