//! Tests of `nuclei::sync::mesh`: the layout of the channels, delivery
//! between parties on separate workers with each sender's order kept, the
//! fairness of `recv`, and the edges (self-sends, parties out of range,
//! ended channels).
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::task::Poll;

use nuclei::runtime::Runtime;
use nuclei::sync::mesh::mesh;
use nuclei::sync::mpsc::error::{SendError, TryRecvError, TrySendError};
use sync_support::{Probe, Task, block_on};

#[test]
fn every_party_knows_its_index_and_the_size_of_the_mesh() {
    let parties = mesh::<u8>(3, 1);
    assert_eq!(parties.len(), 3);
    for (i, party) in parties.iter().enumerate() {
        assert_eq!((party.index(), party.parties()), (i, 3));
    }
}

#[test]
fn an_empty_mesh_has_no_parties() {
    assert!(mesh::<u8>(0, 1).is_empty());
}

#[test]
#[should_panic(expected = "buffer > 0")]
fn a_zero_capacity_panics() {
    let _ = mesh::<u8>(2, 0);
}

#[test]
#[should_panic(expected = "buffer > 0")]
fn a_zero_capacity_panics_even_without_parties() {
    let _ = mesh::<u8>(0, 0);
}

#[test]
fn into_parts_is_indexed_by_peer() {
    let mut parties = mesh::<u8>(3, 1);
    let mut p2 = parties.pop().unwrap();
    let p1 = parties.pop().unwrap();
    let mut p0 = parties.pop().unwrap();
    let (senders, mut receivers) = p1.into_parts();
    assert_eq!((senders.len(), receivers.len()), (3, 3));
    senders[2].try_send(7).unwrap();
    assert_eq!(p2.try_recv(), Ok((1, 7)));
    p0.try_send_to(1, 8).unwrap();
    assert_eq!(receivers[0].try_recv(), Ok(8));
    senders[0].try_send(9).unwrap();
    assert_eq!(p0.try_recv(), Ok((1, 9)));
}

#[test]
fn a_party_sends_to_itself() {
    let mut parties = mesh::<u32>(1, 2);
    let mut party = parties.pop().unwrap();
    block_on(party.send_to(0, 5)).unwrap();
    party.try_send_to(0, 6).unwrap();
    assert_eq!(block_on(party.recv_from(0)), Some(5));
    assert_eq!(block_on(party.recv()), Some((0, 6)));
}

#[test]
fn a_party_out_of_range_is_refused_without_losing_the_value() {
    let mut parties = mesh::<u32>(2, 1);
    let mut party = parties.remove(0);
    assert_eq!(block_on(party.send_to(2, 1)), Err(SendError(1)));
    assert!(matches!(
        party.try_send_to(9, 2),
        Err(TrySendError::Closed(2))
    ));
    assert_eq!(block_on(party.recv_from(2)), None);
    assert!(party.sender(2).is_none());
    assert!(party.receiver(2).is_none());
    assert!(party.sender(1).is_some());
    assert!(party.receiver(1).is_some());
}

#[test]
fn a_full_channel_refuses_try_send_to_and_does_not_hold_up_other_pairs() {
    let mut parties = mesh::<u8>(2, 1);
    let mut p1 = parties.pop().unwrap();
    let p0 = parties.pop().unwrap();
    p0.try_send_to(1, 1).unwrap();
    assert!(matches!(p0.try_send_to(1, 2), Err(TrySendError::Full(2))));
    p0.try_send_to(0, 3).unwrap();
    p1.try_send_to(0, 4).unwrap();
    assert_eq!(p1.try_recv(), Ok((0, 1)));
    p0.try_send_to(1, 2).unwrap();
}

#[test]
fn a_send_waits_for_the_peer_to_take_a_value() {
    let mut parties = mesh::<u8>(2, 1);
    let mut p1 = parties.pop().unwrap();
    let p0 = parties.pop().unwrap();
    p0.try_send_to(1, 1).unwrap();
    let mut send = Task::new(p0.send_to(1, 2));
    send.pending();
    assert_eq!(p1.try_recv(), Ok((0, 1)));
    assert_eq!(send.wakes(), 1);
    send.ready().unwrap();
    assert_eq!(p1.try_recv(), Ok((0, 2)));
}

#[test]
fn recv_serves_a_quiet_peer_after_the_one_that_flooded() {
    let mut parties = mesh::<u32>(3, 8);
    let p2 = parties.pop().unwrap();
    let p1 = parties.pop().unwrap();
    let mut p0 = parties.pop().unwrap();
    for n in 0..4 {
        p1.try_send_to(0, n).unwrap();
    }
    for n in 0..3 {
        assert_eq!(block_on(p0.recv()), Some((1, n)));
    }
    p2.try_send_to(0, 99).unwrap();
    assert_eq!(block_on(p0.recv()), Some((2, 99)));
    assert_eq!(block_on(p0.recv()), Some((1, 3)));
}

#[test]
fn recv_registers_every_peer_and_is_woken_by_any() {
    let mut parties = mesh::<u32>(3, 2);
    let p2 = parties.pop().unwrap();
    let p1 = parties.pop().unwrap();
    let mut p0 = parties.pop().unwrap();
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| p0.poll_recv(cx)).is_pending());
    p2.try_send_to(0, 5).unwrap();
    assert_eq!(probe.wakes(), 1);
    assert_eq!(
        probe.enter(|cx| p0.poll_recv(cx)),
        Poll::Ready(Some((2, 5)))
    );
    assert!(probe.enter(|cx| p0.poll_recv(cx)).is_pending());
    p1.try_send_to(0, 6).unwrap();
    assert_eq!(probe.wakes(), 2);
    assert_eq!(
        probe.enter(|cx| p0.poll_recv(cx)),
        Poll::Ready(Some((1, 6)))
    );
}

#[test]
fn recv_ends_after_every_channel_ended_and_was_drained() {
    let mut parties = mesh::<u32>(3, 4);
    let p2 = parties.pop().unwrap();
    let p1 = parties.pop().unwrap();
    let mut p0 = parties.pop().unwrap();
    p1.try_send_to(0, 1).unwrap();
    p2.try_send_to(0, 2).unwrap();
    p0.try_send_to(0, 3).unwrap();
    drop(p1);
    drop(p2);
    assert_eq!(p0.try_recv(), Ok((0, 3)));
    assert_eq!(p0.try_recv(), Ok((1, 1)));

    let mut recv = Task::new(p0.recv());
    assert_eq!(recv.poll(), Poll::Ready(Some((2, 2))));
    drop(recv);

    let mut recv = Task::new(p0.recv());
    recv.pending();
    drop(recv);
    assert_eq!(p0.try_recv(), Err(TryRecvError::Empty));

    p0.close_senders();
    assert_eq!(block_on(p0.recv()), None);
    assert_eq!(p0.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(block_on(p0.send_to(0, 4)), Err(SendError(4)));
    assert!(p0.sender(0).is_none());
}

#[test]
fn a_pending_recv_is_woken_when_the_last_channel_ends() {
    let mut parties = mesh::<u32>(2, 1);
    let p1 = parties.pop().unwrap();
    let mut p0 = parties.pop().unwrap();
    p0.close_senders();
    let mut recv = Task::new(p0.recv());
    recv.pending();
    drop(p1);
    assert_eq!(recv.wakes(), 1);
    assert_eq!(recv.ready(), None);
}

#[test]
fn debug_names_the_party() {
    let parties = mesh::<u8>(2, 1);
    assert!(format!("{:?}", parties[1]).contains("index: 1"));
}

#[test]
fn four_parties_on_four_workers_receive_exactly_what_was_sent_to_them() {
    const PARTIES: usize = 4;
    const PER_PEER: usize = 500;
    let rt = Runtime::builder()
        .worker_threads(PARTIES)
        .pin_workers(false)
        .build()
        .expect("the runtime starts");
    let receipts = rt.block_on(async {
        let mut receivers = Vec::new();
        let mut senders = Vec::new();
        for mut party in mesh::<(usize, usize)>(PARTIES, 8) {
            let me = party.index();
            let outgoing: Vec<_> = (0..PARTIES)
                .map(|to| party.sender(to).expect("a party").clone())
                .collect();
            senders.push(rt.spawn(async move {
                for seq in 0..PER_PEER {
                    for tx in &outgoing {
                        tx.send((me, seq)).await.expect("the receiver is there");
                    }
                }
            }));
            receivers.push(rt.spawn(async move {
                let mut next = [0usize; PARTIES];
                for _ in 0..PARTIES * PER_PEER {
                    let (from, (sender, seq)) = party.recv().await.expect("values are coming");
                    assert_eq!(from, sender);
                    assert_eq!(seq, next[from], "party {me}: order from {from} broke");
                    next[from] += 1;
                }
                next
            }));
        }
        for sender in senders {
            sender.await;
        }
        let mut receipts = Vec::new();
        for receiver in receivers {
            receipts.push(receiver.await);
        }
        receipts
    });
    assert_eq!(receipts, vec![[PER_PEER; PARTIES]; PARTIES]);
}
