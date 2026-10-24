//! `Proactor::wake` on a process of its own: it ends every wait in
//! progress, which would cut the other tests' waits short.

use std::time::{Duration, Instant};

use nuclei::Proactor;

#[test]
fn a_wake_ends_a_wait_in_progress() {
    let waiter = std::thread::spawn(|| {
        let start = Instant::now();
        Proactor::get()
            .wait(64, Some(Duration::from_secs(30)))
            .unwrap();
        start.elapsed()
    });
    // Keep waking until the waiter is back: it may not be waiting yet.
    while !waiter.is_finished() {
        Proactor::get().wake();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(waiter.join().unwrap() < Duration::from_secs(30));
}
