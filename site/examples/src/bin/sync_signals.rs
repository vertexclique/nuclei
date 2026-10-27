use std::sync::Arc;

use nuclei::sync::{Barrier, Event, Latch, Notify};

#[nuclei::main]
async fn main() {
    // Notify: wake one waiter; a notify with nobody waiting is kept for the next.
    let notify = Arc::new(Notify::new());
    let waiter = {
        let notify = Arc::clone(&notify);
        nuclei::spawn(async move { notify.notified().await })
    };
    notify.notify_one();
    waiter.await;

    // Event: set once, and every wait, now or later, ends.
    let ready = Arc::new(Event::new());
    let watcher = {
        let ready = Arc::clone(&ready);
        nuclei::spawn(async move { ready.wait().await })
    };
    ready.set();
    watcher.await;
    ready.wait().await;

    // Latch: waits end once the count reaches zero.
    let started = Arc::new(Latch::new(3));
    for _ in 0..3 {
        let started = Arc::clone(&started);
        nuclei::spawn(async move { started.count_down() }).detach();
    }
    started.wait().await;

    // Barrier: each wait ends once n tasks are waiting; one is the leader.
    let barrier = Arc::new(Barrier::new(2));
    let other = {
        let barrier = Arc::clone(&barrier);
        nuclei::spawn(async move { barrier.wait().await.is_leader() })
    };
    let mine = barrier.wait().await.is_leader();
    assert!(mine != other.await);
}
