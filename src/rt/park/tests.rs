use std::time::{Duration, Instant};

use super::Poller;

#[test]
fn a_wait_times_out_without_a_wake() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    assert!(
        !poller
            .wait(Some(Duration::from_millis(20)), |_, _| {})
            .unwrap()
    );
    assert!(start.elapsed() >= Duration::from_millis(20), "never early");
}

#[test]
fn a_zero_timeout_only_looks() {
    let mut poller = Poller::new().unwrap();
    assert!(!poller.wait(Some(Duration::ZERO), |_, _| {}).unwrap());
}

#[test]
fn a_wake_before_the_wait_makes_it_return_at_once() {
    let mut poller = Poller::new().unwrap();
    let n = poller.notifier();
    n.notify();
    n.notify();
    assert!(poller.wait(None, |_, _| {}).unwrap());
    // The wakes collapsed: the next wait times out.
    assert!(
        !poller
            .wait(Some(Duration::from_millis(1)), |_, _| {})
            .unwrap()
    );
}

#[test]
fn a_wake_from_another_thread_ends_a_wait() {
    let mut poller = Poller::new().unwrap();
    let n = poller.notifier();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        n.notify();
    });
    assert!(
        poller
            .wait(Some(Duration::from_secs(10)), |_, _| {})
            .unwrap()
    );
    t.join().unwrap();
}

#[test]
fn a_notifier_outliving_its_poller_is_harmless() {
    let poller = Poller::new().unwrap();
    let n = poller.notifier();
    drop(poller);
    n.notify();
}

#[test]
fn a_registered_socket_reports_its_readiness_with_its_token() {
    use std::io::Write;
    let mut poller = Poller::new().unwrap();
    let (mut a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    b.set_nonblocking(true).unwrap();
    use std::os::fd::AsRawFd;
    poller.registrar().add(b.as_raw_fd(), 42).unwrap();
    // Writable at once (an empty send buffer), edge-triggered.
    let mut seen = Vec::new();
    poller
        .wait(Some(Duration::from_millis(100)), |t, r| seen.push((t, r)))
        .unwrap();
    assert!(seen.iter().any(|&(t, r)| t == 42 && r.writable));
    seen.clear();
    a.write_all(b"x").unwrap();
    poller
        .wait(Some(Duration::from_secs(5)), |t, r| seen.push((t, r)))
        .unwrap();
    assert!(seen.iter().any(|&(t, r)| t == 42 && r.readable), "{seen:?}");
    poller.registrar().delete(b.as_raw_fd()).unwrap();
    a.write_all(b"y").unwrap();
    seen.clear();
    poller
        .wait(Some(Duration::from_millis(20)), |t, r| seen.push((t, r)))
        .unwrap();
    assert!(seen.is_empty(), "no event after the registration is gone");
}
