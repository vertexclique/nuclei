//! Notify and the barrier.

use super::*;

// === Notify ===

#[test]
fn notify_one_stores_exactly_one_permit() {
    let notify = Notify::new();
    notify.notify_one();
    notify.notify_one();
    notify.notify_last();
    Polled::new(notify.notified()).ready();
    Polled::new(notify.notified()).pending();
    static NOTIFY: Notify = Notify::const_new();
    NOTIFY.notify_one();
    Polled::new(NOTIFY.notified()).ready();
}

#[test]
fn notify_one_goes_to_a_polled_future_and_not_to_one_never_polled() {
    let notify = Notify::new();
    let never_polled = notify.notified();
    let mut waiting = Polled::new(notify.notified());
    waiting.pending();
    notify.notify_one();
    assert_eq!(waiting.wakes(), 1);
    waiting.ready();
    // The one that was never polled had no claim on it: nothing was stored.
    notify.notify_one();
    drop(never_polled);
    Polled::new(notify.notified()).ready();
    Polled::new(notify.notified()).pending();
}

#[test]
fn notify_waiters_reaches_every_future_created_before_it_polled_or_not() {
    let notify = Notify::new();
    let mut queued = Polled::new(notify.notified());
    queued.pending();
    // Created before the call, first polled after it.
    let mut created = Polled::new(notify.notified());
    notify.notify_waiters();
    assert_eq!(queued.wakes(), 1);
    queued.ready();
    created.ready();
    // Created after the call: not its.
    let mut later = Polled::new(notify.notified());
    later.pending();
    notify.notify_waiters();
    later.ready();
    // It stored no permit.
    Polled::new(notify.notified()).pending();
}

#[test]
fn a_broadcast_does_not_spend_a_stored_permit() {
    let notify = Notify::new();
    notify.notify_one();
    let mut created = Polled::new(notify.notified());
    notify.notify_waiters();
    created.ready();
    Polled::new(notify.notified()).ready();
    Polled::new(notify.notified()).pending();
}

#[test]
fn notify_last_wakes_the_newest_waiter() {
    let notify = Notify::new();
    let mut waiters: Vec<_> = (0..3).map(|_| Polled::new(notify.notified())).collect();
    for waiter in &mut waiters {
        waiter.pending();
    }
    notify.notify_last();
    assert_eq!(
        waiters.iter().map(Polled::wakes).collect::<Vec<_>>(),
        [0, 0, 1]
    );
    notify.notify_one();
    assert_eq!(
        waiters.iter().map(Polled::wakes).collect::<Vec<_>>(),
        [1, 0, 1]
    );
    waiters[2].ready();
    waiters[0].ready();
    waiters[1].pending();
}

#[test]
fn enable_queues_a_future_before_it_is_polled() {
    let notify = Notify::new();
    let mut waiting = pin!(notify.notified());
    assert!(!waiting.as_mut().enable(), "nothing was sent");
    notify.notify_one();
    assert!(
        waiting.as_mut().enable(),
        "the notification found it queued"
    );
    assert!(waiting.as_mut().enable(), "and it stays ready");
    Polled::new(notify.notified()).pending();
    // Enabling consumes a stored permit, once.
    notify.notify_one();
    let mut permit = pin!(notify.notified());
    assert!(permit.as_mut().enable());
    Polled::new(notify.notified()).pending();
    // A broadcast after creation is seen by `enable` too.
    let mut broadcast = pin!(notify.notified());
    notify.notify_waiters();
    assert!(broadcast.as_mut().enable());
}

#[test]
fn a_notified_is_fused() {
    let notify = Notify::new();
    notify.notify_one();
    let mut once = Polled::new(notify.notified());
    once.ready();
    once.ready();
    once.ready();
}

#[test]
fn an_owned_notified_works_like_a_borrowed_one() {
    let notify = Arc::new(Notify::new());
    let mut waiting = Polled::new(Arc::clone(&notify).notified_owned());
    waiting.pending();
    notify.notify_one();
    waiting.ready();
    notify.notify_waiters();
    let mut created = Polled::new(Arc::clone(&notify).notified_owned());
    created.pending();
    notify.notify_waiters();
    created.ready();
    let mut enabled = pin!(Arc::clone(&notify).notified_owned());
    assert!(!enabled.as_mut().enable());
    notify.notify_last();
    assert!(enabled.as_mut().enable());
    // The future owns the `Notify`.
    let owned = Arc::clone(&notify).notified_owned();
    drop(notify);
    let mut owned = pin!(owned);
    assert!(!owned.as_mut().enable());
}

/// The named futures sit at the path the `futures` module gives them too.
#[test]
fn the_notify_futures_are_reachable_through_the_futures_module() {
    fn name(
        _: Option<nuclei::sync::futures::OwnedNotified>,
        _: Option<nuclei::sync::futures::Notified<'static>>,
    ) {
    }
    name(None, None);
}

#[test]
fn a_dropped_notified_passes_the_notification_on() {
    let notify = Notify::new();
    let mut first = Polled::new(notify.notified());
    first.pending();
    let mut second = Polled::new(notify.notified());
    second.pending();
    notify.notify_one();
    drop(first);
    assert_eq!(second.wakes(), 1, "notify_one moved on to the next waiter");
    second.ready();

    let mut oldest = Polled::new(notify.notified());
    oldest.pending();
    let mut newest = Polled::new(notify.notified());
    newest.pending();
    notify.notify_last();
    drop(newest);
    assert_eq!(oldest.wakes(), 1, "notify_last moved on to the newest left");
    oldest.ready();

    let arc = Arc::new(Notify::new());
    let mut owned = Polled::new(Arc::clone(&arc).notified_owned());
    owned.pending();
    let mut behind = Polled::new(arc.notified());
    behind.pending();
    arc.notify_one();
    drop(owned);
    assert_eq!(behind.wakes(), 1);
    behind.ready();
}

#[test]
fn a_waiter_on_another_thread_is_reached() {
    let notify = Arc::new(Notify::new());
    let waiter = {
        let notify = Arc::clone(&notify);
        std::thread::spawn(move || block_on(notify.notified()))
    };
    notify.notify_one();
    waiter.join().expect("waiter");
    let owned = {
        let notify = Arc::clone(&notify);
        std::thread::spawn(move || block_on(notify.notified_owned()))
    };
    // A permit stored before the wait, or a wake after it: either way it ends.
    notify.notify_one();
    owned.join().expect("owned waiter");
}

// === Barrier ===

#[test]
fn a_barrier_releases_groups_with_one_leader_each() {
    let barrier = Barrier::new(3);
    for _ in 0..3 {
        let mut a = Polled::new(barrier.wait());
        a.pending();
        let mut b = Polled::new(barrier.wait());
        b.pending();
        let leader = Polled::new(barrier.wait()).ready();
        assert!(leader.is_leader());
        assert_eq!((a.wakes(), b.wakes()), (1, 1));
        assert!(!a.ready().is_leader() && !b.ready().is_leader());
    }
    let result = Polled::new(Barrier::new(1).wait()).ready();
    assert_eq!(format!("{result:?}"), "BarrierWaitResult { leader: true }");
    assert!(result.clone().is_leader());
}

#[test]
fn a_barrier_of_none_acts_as_one() {
    let barrier = Barrier::new(0);
    for _ in 0..3 {
        assert!(Polled::new(barrier.wait()).ready().is_leader());
    }
}

#[test]
fn a_dropped_wait_stays_counted() {
    let barrier = Barrier::new(2);
    Polled::new(barrier.wait()).pending();
    assert!(Polled::new(barrier.wait()).ready().is_leader());
}

#[test]
fn a_barrier_takes_any_group_size() {
    let barrier = Barrier::new(usize::MAX);
    let mut first = Polled::new(barrier.wait());
    first.pending();
    assert!(format!("{barrier:?}").starts_with("Barrier"));
}

#[test]
fn threads_meet_at_a_barrier() {
    let barrier = Arc::new(Barrier::new(4));
    let leaders: usize = (0..4)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                (0..50)
                    .filter(|_| block_on(barrier.wait()).is_leader())
                    .count()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().expect("worker"))
        .sum();
    assert_eq!(leaders, 50, "one leader per generation");
}
