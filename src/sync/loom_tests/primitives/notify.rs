//! Notify: `notify_one` is never lost, `notify_waiters` and a dropped waiter.

use super::*;

#[test]
fn notify_one_is_never_lost() {
    explore("notify_one_is_never_lost", 3, 5, || {
        let notify = Arc::new(Notify::new());
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_one();
        waiter.join().expect("waiter");
    });
}

#[test]
fn notify_waiters_reaches_a_registered_waiter() {
    explore("notify_waiters_reaches_a_registered_waiter", 3, 3, || {
        let notify = Arc::new(Notify::new());
        let queued = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (notify, flag) = (Arc::clone(&notify), Arc::clone(&queued));
            thread::spawn(move || block_on(flag_when_queued(notify.notified(), &flag)))
        };
        wait_for(&queued);
        notify.notify_waiters();
        waiter.join().expect("waiter");
    });
}

/// A `Notified` created before a `notify_waiters` passes its generation
/// check and is held up before it queues; the broadcast runs, and a newer
/// waiter queues ahead of it. The older one is still owed the broadcast
/// wherever it lands in the queue: a pass that only looks at the front
/// stops at the newer waiter and leaves the older one parked for good,
/// which loom reports as a deadlock.
#[test]
fn an_older_notified_queued_behind_a_newer_one_gets_the_broadcast() {
    explore(
        "an_older_notified_queued_behind_a_newer_one_gets_the_broadcast",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let created = Arc::new(AtomicBool::new(false));
            let older = {
                let (notify, created) = (Arc::clone(&notify), Arc::clone(&created));
                thread::spawn(move || {
                    let waiting = notify.notified();
                    created.store(true, Ordering::Release);
                    block_on(waiting);
                })
            };
            wait_for(&created);
            notify.notify_waiters();
            // Created after the broadcast, so it is not this one's.
            let mut newer = Box::pin(notify.notified());
            let polled = newer
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()));
            assert!(polled.is_pending(), "nothing notified it");
            older.join().expect("older");
            drop(newer);
        },
    );
}

#[test]
fn a_dropped_notified_passes_its_notify_one_on() {
    // SHRUNK: one preemption where there were two (1.2 million interleavings,
    // 105 s; now about 62 thousand), as the other three-thread models run.
    // The scenario is unchanged: the notification still reaches a waiter
    // whether the cancelled future took it, dropped it, or never saw it.
    explore("a_dropped_notified_passes_its_notify_one_on", 1, 5, || {
        let notify = Arc::new(Notify::new());
        // Returns true when its one poll completed, which consumes the
        // notification legitimately rather than dropping it.
        let canceller = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || !poll_once_then_drop(notify.notified()))
        };
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_one();
        if canceller.join().expect("canceller") {
            notify.notify_one();
        }
        waiter.join().expect("waiter");
    });
}

/// A queued `Notified` is dropped on its own thread while a broadcast's
/// wake list takes its waker. The grant let go of the queue's reference,
/// so once the wake list lets go of its own the dropped future frees the
/// node; the wake list must be done with the node by then. Under loom a
/// freed node counts as written (`Waiter::retire`), so a wake list that
/// reads the node's link after letting it go fails this model.
#[test]
fn a_notified_dropped_while_its_broadcast_wakes_it_outlives_the_wake_list() {
    explore(
        "a_notified_dropped_while_its_broadcast_wakes_it_outlives_the_wake_list",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let queued = Arc::new(AtomicBool::new(false));
            let dropper = {
                let (notify, queued) = (Arc::clone(&notify), Arc::clone(&queued));
                thread::spawn(move || {
                    let mut waiting = Box::pin(notify.notified());
                    let polled = waiting
                        .as_mut()
                        .poll(&mut Context::from_waker(std::task::Waker::noop()));
                    assert!(polled.is_pending(), "nothing notified it yet");
                    queued.store(true, Ordering::Release);
                    drop(waiting);
                })
            };
            wait_for(&queued);
            notify.notify_waiters();
            dropper.join().expect("dropper");
        },
    );
}
