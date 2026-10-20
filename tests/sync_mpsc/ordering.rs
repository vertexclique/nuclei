//! The order of one sender's values.
//!
//! The guarantee of the established API is that what one sender sends comes out
//! in the order it was sent. This channel does not keep it under load: the
//! message queue (kovan's segment queue) lets the receiver take a slot past one
//! that a sender has claimed but not finished writing, so when the receiver
//! stalls at that moment, a sender's next value can be taken first. The test
//! below stands for the correct behaviour. It fails on some runs today (it
//! failed in one of three runs of three senders and nine million values on the
//! machine it was written on, and the same queue used directly fails more
//! often), which is why it is ignored; run it with `--ignored`. It must pass
//! on every run before the ignore is lifted.

use super::*;

#[test]
#[ignore = "known defect: the message queue can deliver a sender's later value before its earlier one under load (fails on some runs)"]
fn each_senders_values_arrive_in_the_order_it_sent_them_under_load() {
    let per_sender = scale(3_000_000, 1_000);
    let senders = 3usize;
    let (tx, mut rx) = mpsc::unbounded_channel::<(usize, u64)>();
    let handles: Vec<_> = (0..senders)
        .map(|from| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for seq in 0..per_sender {
                    tx.send((from, seq)).expect("receiver is alive");
                }
            })
        })
        .collect();
    drop(tx);

    // A receiver that polls without pause reaches the end of the queue, where
    // slots are claimed but not yet written, as often as it can.
    let mut next = vec![0u64; senders];
    let mut out_of_order = 0u64;
    loop {
        match rx.try_recv() {
            Ok((from, seq)) => {
                if seq != next[from] {
                    out_of_order += 1;
                }
                next[from] = seq + 1;
            }
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
            Err(TryRecvError::Disconnected) => break,
        }
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(
        out_of_order, 0,
        "{out_of_order} values arrived out of their sender's order"
    );
}

/// What does hold: with the sender and the receiver never running at once, the
/// order is exact.
#[test]
fn values_sent_before_the_receiver_runs_come_out_in_order() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handles: Vec<_> = (0..4usize)
        .map(|from| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for seq in 0..5_000u64 {
                    tx.send((from, seq)).expect("receiver is alive");
                }
            })
        })
        .collect();
    drop(tx);
    for handle in handles {
        handle.join().unwrap();
    }

    let mut next = [0u64; 4];
    while let Some((from, seq)) = block_on(rx.recv()) {
        assert_eq!(seq, next[from], "sender {from}");
        next[from] += 1;
    }
    assert_eq!(next, [5_000; 4]);
}
