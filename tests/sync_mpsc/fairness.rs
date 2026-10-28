//! The bounded channel's capacity is served first come, first served: waiting
//! sends and reservations get their slots in the order they began to wait,
//! a later request never overtakes an earlier one however little it asks for,
//! the `try_` forms never take a slot ahead of a waiter, and a wait that is
//! dropped passes on whatever it was handed, so no slot is ever lost.

use super::*;

/// Fills a channel of `bound` slots with values `0..bound`.
fn full(bound: usize) -> (mpsc::Sender<usize>, mpsc::Receiver<usize>) {
    let (tx, rx) = mpsc::channel(bound);
    for i in 0..bound {
        tx.try_send(i).unwrap();
    }
    (tx, rx)
}

#[test]
fn waiting_senders_are_served_in_the_order_they_began_to_wait() {
    let (tx, mut rx) = full(1);
    let mut senders: Vec<_> = (1..=5).map(|i| Task::new(tx.send(i))).collect();
    for sender in &mut senders {
        sender.pending();
    }
    for (turn, value) in (0..5).zip(0..) {
        // Each value taken frees one slot, which goes to the oldest waiter
        // and to nobody else.
        assert_eq!(rx.try_recv(), Ok(value));
        for (at, sender) in senders.iter().enumerate() {
            assert_eq!(
                sender.wakes(),
                usize::from(at <= turn),
                "turn {turn}: only the senders up to the oldest waiting one were woken"
            );
        }
        assert!(senders[turn].ready().is_ok());
        for later in &mut senders[turn + 1..] {
            later.pending();
        }
    }
    assert_eq!(rx.try_recv(), Ok(5));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn a_later_smaller_reserve_many_does_not_overtake_an_earlier_larger_one() {
    let (tx, mut rx) = full(4);
    let mut large = Task::new(tx.reserve_many(3));
    large.pending();
    let mut small = Task::new(tx.reserve_many(1));
    small.pending();
    let mut single = Task::new(tx.reserve());
    single.pending();

    // One slot comes free: it is set aside for the large request at the front.
    assert_eq!(rx.try_recv(), Ok(0));
    assert_eq!(tx.capacity(), 0, "the free slot is set aside, not free");
    assert!(!large.is_woken() && !small.is_woken() && !single.is_woken());
    assert!(matches!(tx.try_send(9), Err(TrySendError::Full(9))));

    // A second: still not enough for the front, and nothing behind it moves.
    assert_eq!(rx.try_recv(), Ok(1));
    assert!(!small.is_woken() && !single.is_woken());

    // The third completes the large request, and only it.
    assert_eq!(rx.try_recv(), Ok(2));
    assert!(large.is_woken());
    assert!(!small.is_woken() && !single.is_woken());
    let permits = large.ready().unwrap();
    assert_eq!(permits.len(), 3);
    small.pending();
    single.pending();

    // The fourth goes to the small request, then the single one waits on.
    assert_eq!(rx.try_recv(), Ok(3));
    assert!(small.is_woken());
    assert!(!single.is_woken());
    let small_permits = small.ready().unwrap();
    assert_eq!(small_permits.len(), 1);

    // Returning the large request's slots serves the single reservation, and
    // the two left over are free.
    drop(permits);
    assert!(single.is_woken());
    let single_permit = single.ready().unwrap();
    assert_eq!(tx.capacity(), 2);
    drop((small_permits, single_permit));
    assert_eq!(tx.capacity(), 4);
}

#[test]
fn the_try_forms_never_take_a_slot_ahead_of_a_waiter() {
    let (tx, mut rx) = full(1);
    let mut waiting = Task::new(tx.reserve());
    waiting.pending();

    assert!(matches!(tx.try_send(7), Err(TrySendError::Full(7))));
    assert_eq!(rx.try_recv(), Ok(0));
    // The freed slot is the waiter's even before it polls.
    assert!(waiting.is_woken());
    assert_eq!(tx.capacity(), 0);
    assert!(matches!(tx.try_send(7), Err(TrySendError::Full(7))));
    assert!(matches!(tx.try_reserve(), Err(TrySendError::Full(()))));
    assert!(matches!(
        tx.try_reserve_many(1),
        Err(TrySendError::Full(()))
    ));
    match tx.clone().try_reserve_owned() {
        Err(TrySendError::Full(sender)) => assert!(sender.same_channel(&tx)),
        other => panic!("expected Full, got {other:?}"),
    }
    // Asking for nothing never waits.
    assert_eq!(tx.try_reserve_many(0).unwrap().len(), 0);

    waiting.ready().unwrap().send(1);
    assert_eq!(rx.try_recv(), Ok(1));
    tx.try_send(2).unwrap();
}

#[test]
fn a_new_send_queues_behind_a_waiting_one_even_when_a_slot_is_free_for_it() {
    let (tx, mut rx) = full(2);
    let mut large = Task::new(tx.reserve_many(2));
    large.pending();
    assert_eq!(rx.try_recv(), Ok(0));

    // One slot is set aside for the front request; a new send that would fit
    // in it waits behind instead of taking it.
    let mut late = Task::new(tx.send(5));
    late.pending();
    assert_eq!(rx.try_recv(), Ok(1));
    assert!(large.is_woken());
    assert!(!late.is_woken());
    drop(large);
    assert!(late.is_woken(), "the slots of the dropped request move on");
    assert!(late.ready().is_ok());
    assert_eq!(rx.try_recv(), Ok(5));
}

#[test]
fn cancelling_a_waiting_reserve_many_passes_its_set_aside_slots_to_the_next_waiter() {
    let (tx, _rx) = mpsc::channel::<u8>(3);
    let mut held: Vec<_> = tx.try_reserve_many(3).unwrap().collect();
    let mut first = Task::new(tx.reserve_many(3));
    first.pending();
    let mut second = Task::new(tx.reserve_many(2));
    second.pending();

    held.truncate(1);
    assert_eq!(
        tx.capacity(),
        0,
        "two slots are set aside for the first request"
    );
    assert!(!first.is_woken() && !second.is_woken());

    drop(first);
    assert!(
        second.is_woken(),
        "the set-aside slots moved to the next request"
    );
    let permits = second.ready().unwrap();
    assert_eq!(permits.len(), 2);
    assert_eq!(tx.capacity(), 0);

    drop(held);
    assert_eq!(tx.capacity(), 1);
    drop(permits);
    assert_eq!(tx.capacity(), 3);
}

#[test]
fn a_cancelled_send_that_was_already_handed_its_slot_passes_it_on() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(drops.value()).unwrap();
    let mut first = Task::new(tx.send(drops.value()));
    first.pending();
    let mut second = Task::new(tx.send(drops.value()));
    second.pending();

    drop(rx.try_recv().unwrap());
    assert!(first.is_woken(), "the slot went to the first waiting send");
    assert!(!second.is_woken());

    drop(first);
    assert_eq!(drops.count(), 2, "the dropped send dropped its value");
    assert!(second.is_woken(), "and its slot went on to the next one");
    assert!(second.ready().is_ok());
    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 3);
    assert_eq!(tx.capacity(), 1);
}

#[test]
fn a_reservation_dropped_after_its_grant_leaves_nothing_held() {
    let (tx, _rx) = mpsc::channel::<()>(2);
    let held = tx.try_reserve_many(2).unwrap();
    let mut waiting = Task::new(tx.reserve_many(2));
    waiting.pending();
    drop(held);
    assert!(waiting.is_woken());
    assert_eq!(tx.capacity(), 0, "the grant holds both slots");
    drop(waiting);
    assert_eq!(tx.capacity(), 2);
    assert_eq!(tx.try_reserve_many(2).unwrap().len(), 2);
}

#[test]
fn the_capacity_is_whole_after_many_cancelled_waits() {
    const BOUND: usize = 4;
    let (tx, mut rx) = mpsc::channel::<usize>(BOUND);
    for round in 0..500usize {
        // Fill the channel with a mix of values and held permits.
        let queued = round % (BOUND + 1);
        for i in 0..queued {
            tx.try_send(i).unwrap();
        }
        let held = tx.try_reserve_many(BOUND - queued).unwrap();

        // Queue waits of every size, in a pattern that changes each round.
        let mut waits: Vec<_> = (0..6)
            .map(|at| Task::new(tx.reserve_many(1 + (at + round) % BOUND)))
            .collect();
        for wait in &mut waits {
            wait.pending();
        }

        // Free some slots, then drop the waits in one of three orders, some
        // of them after their grant and before they polled.
        let freed = (round / 3) % (queued + 1);
        for _ in 0..freed {
            rx.try_recv().unwrap();
        }
        match round % 3 {
            0 => drop(waits),
            1 => {
                let back = waits.split_off(3);
                drop(back);
                drop(waits);
            }
            _ => {
                waits.reverse();
                drop(waits);
            }
        }
        drop(held);
        while rx.try_recv().is_ok() {}
        assert_eq!(
            tx.capacity(),
            BOUND,
            "round {round}: a slot was lost or made up"
        );
        assert_eq!(
            tx.try_reserve_many(BOUND).unwrap().len(),
            BOUND,
            "round {round}"
        );
    }
}

#[test]
fn reservations_of_every_size_racing_across_threads_leave_the_capacity_whole() {
    const BOUND: usize = 8;
    let rounds = scale(2_000, 50) as usize;
    let (tx, mut rx) = mpsc::channel::<usize>(BOUND);
    let workers: Vec<_> = (0..4)
        .map(|id| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for round in 0..rounds {
                    let n = 1 + (id + round) % BOUND;
                    let mut permits = block_on(tx.reserve_many(n)).expect("the receiver is alive");
                    // Send through some of the slots, give the rest back.
                    for _ in 0..(round % (n + 1)) {
                        permits.next().expect("a permit per slot").send(id);
                    }
                }
            })
        })
        .collect();
    drop(tx);
    let mut received = 0usize;
    while block_on(rx.recv()).is_some() {
        received += 1;
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let expected: usize = (0..4)
        .map(|id| {
            (0..rounds)
                .map(|round| round % (2 + (id + round) % BOUND))
                .sum::<usize>()
        })
        .sum();
    assert_eq!(
        received, expected,
        "every value sent through a permit arrived once"
    );
    assert_eq!(rx.capacity(), BOUND);
}
