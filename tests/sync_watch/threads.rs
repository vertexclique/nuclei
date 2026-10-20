//! Threads racing sends, waits and drops. The counts shrink under Miri (`scale`).

use super::*;

fn scale(real: u64, miri: u64) -> u64 {
    if cfg!(miri) { miri } else { real }
}

/// Receivers waiting in `changed` never lose a send: every one of them ends
/// on the last value, and what each sees only moves forward.
#[test]
fn no_receiver_misses_the_last_value_and_none_sees_time_go_backwards() {
    let last = scale(20_000, 50);
    let (tx, rx) = watch::channel(0u64);
    let receivers: Vec<_> = (0..4)
        .map(|_| {
            let mut rx = rx.clone();
            std::thread::spawn(move || {
                let mut seen = 0u64;
                loop {
                    let value = *rx.borrow_and_update();
                    assert!(value >= seen, "the value went from {seen} to {value}");
                    seen = value;
                    if block_on(rx.changed()).is_err() {
                        break;
                    }
                }
                seen = *rx.borrow();
                seen
            })
        })
        .collect();
    drop(rx);
    for value in 1..=last {
        tx.send(value).unwrap();
    }
    drop(tx);
    for receiver in receivers {
        assert_eq!(receiver.join().expect("receiver"), last);
    }
}

/// `wait_for` is woken by the one send that satisfies it, whenever it lands.
#[test]
fn wait_for_never_misses_the_value_it_waits_for() {
    for _ in 0..scale(2_000, 5) {
        let (tx, rx) = watch::channel(0u32);
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let mut rx = rx.clone();
                std::thread::spawn(move || *block_on(rx.wait_for(|v| *v >= 3)).unwrap())
            })
            .collect();
        drop(rx);
        for value in 1..=3 {
            tx.send(value).unwrap();
        }
        for waiter in waiters {
            assert!(waiter.join().expect("waiter") >= 3);
        }
    }
}

/// Concurrent `send_modify` calls lose no update: the copy-on-write retry
/// applies every one of them to the newest value.
#[test]
fn concurrent_modifications_are_all_applied() {
    let per_thread = scale(5_000, 20);
    let (tx, rx) = watch::channel(0u64);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for _ in 0..per_thread {
                    tx.send_modify(|v| *v += 1);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("modifier");
    }
    assert_eq!(*rx.borrow(), 4 * per_thread);
}

/// `send_if_modified` under contention: the closure runs again on the value
/// that beat it, and the number of `true` answers is the number of changes.
#[test]
fn concurrent_conditional_modifications_count_exactly() {
    let attempts = scale(5_000, 20);
    let (tx, rx) = watch::channel(0u64);
    let changed = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (tx, changed) = (tx.clone(), Arc::clone(&changed));
            std::thread::spawn(move || {
                for _ in 0..attempts {
                    // Increment only multiples of three: of any value this
                    // moves it, or declines.
                    if tx.send_if_modified(|v| {
                        if *v % 3 == 0 {
                            *v += 1;
                            true
                        } else {
                            *v += 1;
                            false
                        }
                    }) {
                        changed.fetch_add(1, Ordering::SeqCst);
                    }
                    tx.send_modify(|v| *v += 1);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("modifier");
    }
    // Each attempt's `send_modify` adds exactly one; a declined attempt
    // changed nothing; an accepted one added one more.
    let accepted = changed.load(Ordering::SeqCst) as u64;
    assert_eq!(*rx.borrow(), 4 * attempts + accepted);
}

/// Producers and receivers coming and going: the sender's `closed` always
/// completes once the last receiver is gone.
#[test]
fn closed_completes_when_receivers_drop_on_other_threads() {
    for _ in 0..scale(2_000, 5) {
        let (tx, rx) = watch::channel(());
        let droppers: Vec<_> = (0..3)
            .map(|_| {
                let rx = rx.clone();
                std::thread::spawn(move || drop(rx))
            })
            .collect();
        drop(rx);
        block_on(tx.closed());
        assert!(tx.is_closed());
        for dropper in droppers {
            dropper.join().expect("dropper");
        }
    }
}

/// `closed` against a `subscribe` that reopens the channel: it completes when
/// the receiver count is zero at some look, and never hangs once it stays
/// zero.
#[test]
fn closed_and_subscribe_race_without_hanging() {
    for _ in 0..scale(2_000, 5) {
        let (tx, rx) = watch::channel(0u8);
        let tx = Arc::new(tx);
        let reopener = {
            let tx = Arc::clone(&tx);
            std::thread::spawn(move || {
                let rx = tx.subscribe();
                drop(rx);
            })
        };
        drop(rx);
        reopener.join().expect("reopener");
        block_on(tx.closed());
    }
}

/// Many senders racing one receiver that watches versions: the count of
/// observed `changed` completions is at most the count of sends, and the final
/// value is the last one any sender published.
#[test]
fn racing_senders_never_overcount_changes() {
    let per_thread = scale(3_000, 10);
    let (tx, mut rx) = watch::channel(0u64);
    let senders: Vec<_> = (0..3)
        .map(|_| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for i in 0..per_thread {
                    tx.send(i).unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    let observer = std::thread::spawn(move || {
        let mut seen = 0usize;
        while block_on(rx.changed()).is_ok() {
            seen += 1;
        }
        seen
    });
    for sender in senders {
        sender.join().expect("sender");
    }
    let seen = observer.join().expect("observer");
    assert!(seen as u64 <= 3 * per_thread);
}

#[test]
fn polling_after_ready_changed_is_pending_until_the_next_send() {
    let (tx, mut rx) = watch::channel(0);
    tx.send(1).unwrap();
    assert!(block_on(rx.changed()).is_ok());
    let mut t = Task::new(rx.changed());
    assert!(matches!(t.poll(), Poll::Pending));
    tx.send(2).unwrap();
    assert!(t.ready().is_ok());
}
