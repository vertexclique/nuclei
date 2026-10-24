//! A sequential model: any sequence of calls leaves the channel agreeing with a plain model.

use super::*;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Send(i32),
    SendReplace(i32),
    SendModify(i32),
    SendIfModified(i32, bool),
    BorrowAndUpdate(usize),
    Borrow(usize),
    HasChanged(usize),
    MarkChanged(usize),
    MarkUnchanged(usize),
    PollChanged(usize),
    CloneRx(usize),
    Subscribe,
    DropRx(usize),
    CloneTx,
    DropTx,
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    let op = prop_oneof![
        3 => any::<i32>().prop_map(Op::Send),
        1 => any::<i32>().prop_map(Op::SendReplace),
        1 => (-5..5).prop_map(Op::SendModify),
        1 => ((-5..5), any::<bool>()).prop_map(|(k, r)| Op::SendIfModified(k, r)),
        3 => (0..8usize).prop_map(Op::BorrowAndUpdate),
        2 => (0..8usize).prop_map(Op::Borrow),
        3 => (0..8usize).prop_map(Op::HasChanged),
        1 => (0..8usize).prop_map(Op::MarkChanged),
        1 => (0..8usize).prop_map(Op::MarkUnchanged),
        3 => (0..8usize).prop_map(Op::PollChanged),
        1 => (0..8usize).prop_map(Op::CloneRx),
        1 => Just(Op::Subscribe),
        1 => (0..8usize).prop_map(Op::DropRx),
        1 => Just(Op::CloneTx),
        1 => Just(Op::DropTx),
    ];
    proptest::collection::vec(op, 0..120)
}

proptest! {
    /// Any sequence of calls leaves the channel agreeing with a plain
    /// model: the value, who has seen which send, when it is closed, and
    /// what each call returns.
    #[test]
    fn the_channel_agrees_with_a_sequential_model(ops in ops()) {
        let (tx, rx) = watch::channel(0i32);
        let mut txs = vec![tx];
        // (receiver, the model version it has seen)
        let mut rxs: Vec<(watch::Receiver<i32>, u64)> = vec![(rx, 0)];
        let mut current = 0i32;
        let mut version = 0u64;

        for op in ops {
            let pick = |i: usize, len: usize| i % len.max(1);
            match op {
                Op::Send(v) if !txs.is_empty() => {
                    let sent = txs[0].send(v);
                    if rxs.is_empty() {
                        prop_assert_eq!(sent.unwrap_err().0, v);
                    } else {
                        prop_assert!(sent.is_ok());
                        current = v;
                        version += 1;
                    }
                }
                Op::SendReplace(v) if !txs.is_empty() => {
                    prop_assert_eq!(txs[0].send_replace(v), current);
                    current = v;
                    version += 1;
                }
                Op::SendModify(k) if !txs.is_empty() => {
                    txs[0].send_modify(|n| *n = n.wrapping_add(k));
                    current = current.wrapping_add(k);
                    version += 1;
                }
                Op::SendIfModified(k, publish) if !txs.is_empty() => {
                    let sent = txs[0].send_if_modified(|n| {
                        *n = n.wrapping_add(k);
                        publish
                    });
                    prop_assert_eq!(sent, publish);
                    if publish {
                        current = current.wrapping_add(k);
                        version += 1;
                    }
                }
                Op::BorrowAndUpdate(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    let (rx, seen) = &mut rxs[i];
                    let borrowed = rx.borrow_and_update();
                    prop_assert_eq!(*borrowed, current);
                    prop_assert_eq!(borrowed.has_changed(), *seen != version);
                    *seen = version;
                }
                Op::Borrow(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    let (rx, seen) = &rxs[i];
                    let borrowed = rx.borrow();
                    prop_assert_eq!(*borrowed, current);
                    prop_assert_eq!(borrowed.has_changed(), *seen != version);
                }
                Op::HasChanged(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    let (rx, seen) = &rxs[i];
                    if txs.is_empty() {
                        prop_assert!(rx.has_changed().is_err());
                    } else {
                        prop_assert_eq!(rx.has_changed().ok(), Some(*seen != version));
                    }
                }
                Op::MarkChanged(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    rxs[i].0.mark_changed();
                    rxs[i].1 = rxs[i].1.wrapping_sub(1);
                }
                Op::MarkUnchanged(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    rxs[i].0.mark_unchanged();
                    rxs[i].1 = version;
                }
                Op::PollChanged(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    let (rx, seen) = &mut rxs[i];
                    let polled = Task::new(rx.changed()).poll();
                    if *seen != version {
                        prop_assert!(matches!(polled, Poll::Ready(Ok(()))));
                        *seen = version;
                    } else if txs.is_empty() {
                        prop_assert!(matches!(polled, Poll::Ready(Err(_))));
                    } else {
                        prop_assert!(polled.is_pending());
                    }
                }
                Op::CloneRx(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    let copy = rxs[i].0.clone();
                    let seen = rxs[i].1;
                    rxs.push((copy, seen));
                }
                Op::Subscribe if !txs.is_empty() => {
                    rxs.push((txs[0].subscribe(), version));
                }
                Op::DropRx(i) if !rxs.is_empty() => {
                    let i = pick(i, rxs.len());
                    drop(rxs.remove(i));
                }
                Op::CloneTx if !txs.is_empty() => {
                    let copy = txs[0].clone();
                    txs.push(copy);
                }
                Op::DropTx if !txs.is_empty() => {
                    txs.pop();
                }
                _ => {}
            }
            if let Some(tx) = txs.first() {
                prop_assert_eq!(*tx.borrow(), current);
                prop_assert_eq!(tx.receiver_count(), rxs.len());
                prop_assert_eq!(tx.sender_count(), txs.len());
                prop_assert_eq!(tx.is_closed(), rxs.is_empty());
            }
            for (rx, _) in &rxs {
                prop_assert_eq!(*rx.borrow(), current);
                prop_assert_eq!(rx.is_closed(), txs.is_empty());
            }
        }
    }
}
