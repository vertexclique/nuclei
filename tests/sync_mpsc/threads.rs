//! Threads racing sends, receives, closes and drops: every value arrives
//! exactly once, the capacity is never exceeded, no wakeup is lost, and every
//! value is dropped exactly once whatever the race. (The order of one sender's
//! values under load is in `ordering.rs`.) The counts shrink under Miri
//! (`scale`).

use super::*;

use std::sync::atomic::AtomicBool;

/// A message: who sent it, which one it is, and a token that counts the
/// values alive at once.
struct Msg {
    from: usize,
    seq: u64,
    _token: Token,
}

/// Counts the values alive: created when a message is made, gone when it is
/// dropped.
struct Token {
    live: Arc<AtomicUsize>,
}

impl Token {
    fn new(live: &Arc<AtomicUsize>, max: &AtomicUsize) -> Self {
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
        Token {
            live: Arc::clone(live),
        }
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a receiver checks of the messages it takes: every sender's values
/// arrive without a gap or a repeat.
struct Ledger {
    seen: Vec<Vec<bool>>,
}

impl Ledger {
    fn new(senders: usize, per_sender: u64) -> Self {
        Self {
            seen: vec![vec![false; per_sender as usize]; senders],
        }
    }

    fn take(&mut self, msg: &Msg) {
        let seen = &mut self.seen[msg.from][msg.seq as usize];
        assert!(!*seen, "sender {} delivered {} twice", msg.from, msg.seq);
        *seen = true;
    }

    fn assert_complete(&self) {
        for (from, seen) in self.seen.iter().enumerate() {
            if let Some(seq) = seen.iter().position(|got| !got) {
                panic!("sender {from} lost value {seq}");
            }
        }
    }
}

#[test]
fn bounded_senders_deliver_every_value_once_without_exceeding_the_buffer() {
    let per_sender = scale(4_000, 20);
    for (senders, buffer) in [(1usize, 1usize), (4, 1), (8, 3), (8, 64)] {
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel::<Msg>(buffer);

        let handles: Vec<_> = (0..senders)
            .map(|from| {
                let (tx, live, max_live) = (tx.clone(), live.clone(), max_live.clone());
                std::thread::spawn(move || {
                    for seq in 0..per_sender {
                        // The slot is held before the message is made, so the
                        // live count measures slots used.
                        let permit = block_on(tx.reserve()).expect("receiver is alive");
                        permit.send(Msg {
                            from,
                            seq,
                            _token: Token::new(&live, &max_live),
                        });
                    }
                })
            })
            .collect();
        drop(tx);

        let mut ledger = Ledger::new(senders, per_sender);
        while let Some(msg) = block_on(rx.recv()) {
            ledger.take(&msg);
        }
        ledger.assert_complete();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(live.load(Ordering::SeqCst), 0);
        // The slots held plus the one value in the receiver's hand: the
        // receiver frees a value's slot when it takes the value, before it
        // drops the message.
        let max = max_live.load(Ordering::SeqCst);
        assert!(
            max <= buffer + 1,
            "{max} values alive at once with a buffer of {buffer}"
        );
    }
}

#[test]
fn a_full_buffer_is_reached_and_never_passed_under_try_send() {
    let senders = 4usize;
    let per_sender = scale(3_000, 20);
    let buffer = 4usize;
    let (tx, mut rx) = mpsc::channel::<Msg>(buffer);
    let live = Arc::new(AtomicUsize::new(0));
    let max_live = Arc::new(AtomicUsize::new(0));
    let full = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..senders)
        .map(|from| {
            let (tx, live, max_live, full) =
                (tx.clone(), live.clone(), max_live.clone(), full.clone());
            std::thread::spawn(move || {
                for seq in 0..per_sender {
                    loop {
                        match tx.try_reserve() {
                            Ok(permit) => {
                                permit.send(Msg {
                                    from,
                                    seq,
                                    _token: Token::new(&live, &max_live),
                                });
                                break;
                            }
                            Err(TrySendError::Full(())) => {
                                full.fetch_add(1, Ordering::Relaxed);
                                std::thread::yield_now();
                            }
                            Err(TrySendError::Closed(())) => panic!("closed"),
                        }
                    }
                }
            })
        })
        .collect();
    drop(tx);

    let mut ledger = Ledger::new(senders, per_sender);
    while let Some(msg) = block_on(rx.recv()) {
        ledger.take(&msg);
    }
    ledger.assert_complete();
    for handle in handles {
        handle.join().unwrap();
    }
    let max = max_live.load(Ordering::SeqCst);
    assert!(
        max <= buffer + 1,
        "{max} values alive with a buffer of {buffer}"
    );
    assert!(
        full.load(Ordering::Relaxed) > 0 || cfg!(miri),
        "senders outran a buffer of {buffer} at least once, so the bound was exercised"
    );
}

#[test]
fn unbounded_senders_deliver_every_value_once() {
    let per_sender = scale(20_000, 50);
    for senders in [1usize, 4, 8] {
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

        let handles: Vec<_> = (0..senders)
            .map(|from| {
                let (tx, live, max_live) = (tx.clone(), live.clone(), max_live.clone());
                std::thread::spawn(move || {
                    for seq in 0..per_sender {
                        tx.send(Msg {
                            from,
                            seq,
                            _token: Token::new(&live, &max_live),
                        })
                        .map_err(|_| ())
                        .expect("receiver is alive");
                    }
                })
            })
            .collect();
        drop(tx);

        let mut ledger = Ledger::new(senders, per_sender);
        while let Some(msg) = block_on(rx.recv()) {
            ledger.take(&msg);
        }
        ledger.assert_complete();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn receiving_in_batches_loses_nothing_either() {
    let per_sender = scale(10_000, 50);
    let senders = 4usize;
    for bounded in [true, false] {
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(AtomicUsize::new(0));
        let mut ledger = Ledger::new(senders, per_sender);
        let mut batch = Vec::new();

        if bounded {
            let (tx, mut rx) = mpsc::channel::<Msg>(16);
            let handles: Vec<_> = (0..senders)
                .map(|from| {
                    let (tx, live, max_live) = (tx.clone(), live.clone(), max_live.clone());
                    std::thread::spawn(move || {
                        for seq in 0..per_sender {
                            let permit = block_on(tx.reserve()).unwrap();
                            permit.send(Msg {
                                from,
                                seq,
                                _token: Token::new(&live, &max_live),
                            });
                        }
                    })
                })
                .collect();
            drop(tx);
            while block_on(rx.recv_many(&mut batch, 7)) != 0 {
                assert!(batch.len() <= 7);
                batch.drain(..).for_each(|msg| ledger.take(&msg));
            }
            handles.into_iter().for_each(|h| h.join().unwrap());
        } else {
            let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
            let handles: Vec<_> = (0..senders)
                .map(|from| {
                    let (tx, live, max_live) = (tx.clone(), live.clone(), max_live.clone());
                    std::thread::spawn(move || {
                        for seq in 0..per_sender {
                            let msg = Msg {
                                from,
                                seq,
                                _token: Token::new(&live, &max_live),
                            };
                            assert!(tx.send(msg).is_ok());
                        }
                    })
                })
                .collect();
            drop(tx);
            while block_on(rx.recv_many(&mut batch, 64)) != 0 {
                assert!(batch.len() <= 64);
                batch.drain(..).for_each(|msg| ledger.take(&msg));
            }
            handles.into_iter().for_each(|h| h.join().unwrap());
        }
        ledger.assert_complete();
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn a_ping_pong_between_two_threads_never_loses_a_wakeup() {
    let rounds = scale(20_000, 100);

    let (ping_tx, mut ping_rx) = mpsc::channel::<u64>(1);
    let (pong_tx, mut pong_rx) = mpsc::channel::<u64>(1);
    let echo = std::thread::spawn(move || {
        while let Some(n) = block_on(ping_rx.recv()) {
            if block_on(pong_tx.send(n + 1)).is_err() {
                break;
            }
        }
    });
    for n in 0..rounds {
        block_on(ping_tx.send(n)).unwrap();
        assert_eq!(block_on(pong_rx.recv()), Some(n + 1));
    }
    drop(ping_tx);
    echo.join().unwrap();

    let (ping_tx, mut ping_rx) = mpsc::unbounded_channel::<u64>();
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<u64>();
    let echo = std::thread::spawn(move || {
        while let Some(n) = block_on(ping_rx.recv()) {
            if pong_tx.send(n + 1).is_err() {
                break;
            }
        }
    });
    for n in 0..rounds {
        ping_tx.send(n).unwrap();
        assert_eq!(block_on(pong_rx.recv()), Some(n + 1));
    }
    drop(ping_tx);
    echo.join().unwrap();
}

#[test]
fn the_last_sender_dropping_never_strands_a_waiting_receiver() {
    for _ in 0..scale(2_000, 20) {
        let (tx, mut rx) = mpsc::channel::<u8>(1);
        let dropper = std::thread::spawn(move || drop(tx));
        assert_eq!(block_on(rx.recv()), None);
        dropper.join().unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
        let sender = std::thread::spawn(move || {
            tx.send(1).unwrap();
        });
        assert_eq!(block_on(rx.recv()), Some(1));
        assert_eq!(block_on(rx.recv()), None);
        sender.join().unwrap();
    }
}

/// The receiver closes while senders are mid-send, then receives until `None`.
/// Every value that a send accepted is received; every one that was refused
/// came back to its sender; each is dropped exactly once.
#[test]
fn closing_while_senders_run_loses_no_accepted_value() {
    for bounded in [true, false] {
        for _ in 0..scale(30, 3) {
            let drops = Drops::new();
            let created = Arc::new(AtomicUsize::new(0));
            let accepted = Arc::new(AtomicUsize::new(0));
            let senders = 4;

            let (tx_b, mut rx_b) = mpsc::channel::<Counted>(3);
            let (tx_u, mut rx_u) = mpsc::unbounded_channel::<Counted>();

            let handles: Vec<_> = (0..senders)
                .map(|_| {
                    let (drops, created, accepted) =
                        (drops.clone(), created.clone(), accepted.clone());
                    let (tx_b, tx_u) = (tx_b.clone(), tx_u.clone());
                    std::thread::spawn(move || {
                        loop {
                            created.fetch_add(1, Ordering::SeqCst);
                            let sent = if bounded {
                                block_on(tx_b.send(drops.value())).map_err(|e| e.0)
                            } else {
                                tx_u.send(drops.value()).map_err(|e| e.0)
                            };
                            match sent {
                                Ok(()) => {
                                    accepted.fetch_add(1, Ordering::SeqCst);
                                }
                                Err(back) => {
                                    drop(back);
                                    break;
                                }
                            }
                        }
                    })
                })
                .collect();
            drop((tx_b, tx_u));

            let mut received = 0usize;
            // Take a few, close in the middle of the traffic, drain to the end.
            for _ in 0..20 {
                let got = if bounded {
                    block_on(rx_b.recv())
                } else {
                    block_on(rx_u.recv())
                };
                assert!(got.is_some());
                received += 1;
            }
            if bounded {
                rx_b.close();
                while block_on(rx_b.recv()).is_some() {
                    received += 1;
                }
            } else {
                rx_u.close();
                while block_on(rx_u.recv()).is_some() {
                    received += 1;
                }
            }
            for handle in handles {
                handle.join().unwrap();
            }
            drop((rx_b, rx_u));

            assert_eq!(
                received,
                accepted.load(Ordering::SeqCst),
                "bounded: {bounded}: a value a send accepted was lost or invented"
            );
            assert_eq!(
                drops.count(),
                created.load(Ordering::SeqCst),
                "bounded: {bounded}: every value made is dropped exactly once"
            );
        }
    }
}

/// The receiver is dropped at some point in the traffic. The values it never
/// took are dropped by it or, for a send that raced the drop, with the
/// channel; none is dropped twice, none leaks.
#[test]
fn dropping_the_receiver_while_senders_run_drops_every_value_exactly_once() {
    for bounded in [true, false] {
        for _ in 0..scale(50, 3) {
            let drops = Drops::new();
            let created = Arc::new(AtomicUsize::new(0));
            let (tx_b, mut rx_b) = mpsc::channel::<Counted>(4);
            let (tx_u, mut rx_u) = mpsc::unbounded_channel::<Counted>();

            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let (drops, created) = (drops.clone(), created.clone());
                    let (tx_b, tx_u) = (tx_b.clone(), tx_u.clone());
                    std::thread::spawn(move || {
                        loop {
                            created.fetch_add(1, Ordering::SeqCst);
                            let refused = if bounded {
                                block_on(tx_b.send(drops.value())).is_err()
                            } else {
                                tx_u.send(drops.value()).is_err()
                            };
                            if refused {
                                break;
                            }
                        }
                    })
                })
                .collect();

            for _ in 0..10 {
                if bounded {
                    assert!(block_on(rx_b.recv()).is_some());
                } else {
                    assert!(block_on(rx_u.recv()).is_some());
                }
            }
            drop((rx_b, rx_u));
            for handle in handles {
                handle.join().unwrap();
            }
            drop((tx_b, tx_u));

            assert_eq!(
                drops.count(),
                created.load(Ordering::SeqCst),
                "bounded: {bounded}"
            );
        }
    }
}

#[test]
fn closed_wakes_a_sender_on_another_thread_when_the_receiver_drops() {
    for _ in 0..scale(500, 10) {
        let (tx, rx) = mpsc::channel::<()>(1);
        let tx2 = tx.clone();
        let waiting = std::thread::spawn(move || block_on(tx2.closed()));
        let unbounded = mpsc::unbounded_channel::<()>();
        let (utx, urx) = unbounded;
        let uwaiting = std::thread::spawn({
            let utx = utx.clone();
            move || block_on(utx.closed())
        });
        drop(rx);
        drop(urx);
        waiting.join().unwrap();
        uwaiting.join().unwrap();
        block_on(tx.closed());
    }
}

#[test]
fn many_tasks_waiting_for_one_slot_each_get_it_in_turn() {
    let waiters = scale(32, 4) as usize;
    let (tx, mut rx) = mpsc::channel::<usize>(1);
    let started = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..waiters)
        .map(|id| {
            let tx = tx.clone();
            let started = started.clone();
            std::thread::spawn(move || {
                while !started.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                block_on(tx.send(id)).unwrap();
            })
        })
        .collect();
    drop(tx);
    started.store(true, Ordering::Release);

    let mut seen = vec![false; waiters];
    while let Some(id) = block_on(rx.recv()) {
        assert!(!seen[id], "value {id} arrived twice");
        seen[id] = true;
    }
    assert!(
        seen.iter().all(|got| *got),
        "a waiting sender was never served"
    );
    handles.into_iter().for_each(|h| h.join().unwrap());
}

#[test]
fn a_receiver_that_moves_between_threads_keeps_its_place() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..100 {
        tx.send(i).unwrap();
    }
    for chunk in 0..10 {
        let mut moved = rx;
        let handle = std::thread::spawn(move || {
            for i in 0..10 {
                assert_eq!(moved.try_recv(), Ok(chunk * 10 + i));
            }
            moved
        });
        rx = handle.join().unwrap();
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}
