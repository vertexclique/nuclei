use std::time::{Duration, Instant};

use super::Poller;

#[test]
fn a_wait_times_out_without_a_wake() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    assert!(!poller.wait(Some(Duration::from_millis(20))).unwrap());
    assert!(start.elapsed() >= Duration::from_millis(20), "never early");
}

#[test]
fn a_zero_timeout_only_looks() {
    let mut poller = Poller::new().unwrap();
    assert!(!poller.wait(Some(Duration::ZERO)).unwrap());
}

#[test]
fn a_wake_before_the_wait_makes_it_return_at_once() {
    let mut poller = Poller::new().unwrap();
    let n = poller.notifier();
    n.notify();
    n.notify();
    assert!(poller.wait(None).unwrap());
    // The wakes collapsed: the next wait times out.
    assert!(!poller.wait(Some(Duration::from_millis(1))).unwrap());
}

#[test]
fn a_wake_from_another_thread_ends_a_wait() {
    let mut poller = Poller::new().unwrap();
    let n = poller.notifier();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        n.notify();
    });
    assert!(poller.wait(Some(Duration::from_secs(10))).unwrap());
    t.join().unwrap();
}

#[test]
fn a_notifier_outliving_its_poller_is_harmless() {
    let poller = Poller::new().unwrap();
    let n = poller.notifier();
    drop(poller);
    n.notify();
}
