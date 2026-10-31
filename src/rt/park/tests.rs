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
    poller.registrar().add(b.as_raw_fd(), 42, false).unwrap();
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
    poller.registrar().delete(b.as_raw_fd(), false).unwrap();
    a.write_all(b"y").unwrap();
    seen.clear();
    poller
        .wait(Some(Duration::from_millis(20)), |t, r| seen.push((t, r)))
        .unwrap();
    assert!(seen.is_empty(), "no event after the registration is gone");
}

/// A TCP socket registered for urgent data hears of an out-of-band byte as
/// urgent and readable, on the pollers that report it.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
#[test]
fn urgent_data_on_a_tcp_socket_is_reported() {
    use std::os::fd::AsRawFd;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mine, _) = listener.accept().unwrap();
    mine.set_nonblocking(true).unwrap();
    let mut poller = Poller::new().unwrap();
    poller.registrar().add(mine.as_raw_fd(), 7, true).unwrap();
    // SAFETY: a plain send of one byte from our own buffer.
    let sent = unsafe { libc::send(peer.as_raw_fd(), b"!".as_ptr().cast(), 1, libc::MSG_OOB) };
    assert_eq!(sent, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut urgent = false;
    while !urgent {
        assert!(Instant::now() < deadline, "no urgent readiness came");
        poller
            .wait(Some(Duration::from_millis(100)), |t, r| {
                urgent |= t == 7 && r.urgent && r.readable;
            })
            .unwrap();
    }
    poller.registrar().delete(mine.as_raw_fd(), true).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "android", precise_timer))]
#[test]
fn a_precise_deadline_ends_a_wait_whose_timeout_is_far() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    let after = Duration::from_millis(3);
    poller.arm_precise(start + after, after);
    poller
        .wait(Some(Duration::from_secs(10)), |_, _| {})
        .unwrap();
    let took = start.elapsed();
    assert!(took >= after, "never early: {took:?}");
    assert!(
        took < Duration::from_secs(5),
        "the deadline ended it: {took:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "android", precise_timer))]
#[test]
fn an_earlier_precise_deadline_moves_the_timer() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    poller.arm_precise(start + Duration::from_secs(3), Duration::from_secs(3));
    let after = Duration::from_millis(2);
    poller.arm_precise(start + after, after);
    poller
        .wait(Some(Duration::from_secs(10)), |_, _| {})
        .unwrap();
    let took = start.elapsed();
    assert!(took >= after, "never early: {took:?}");
    assert!(
        took < Duration::from_secs(2),
        "the earlier deadline ended it: {took:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "android", precise_timer))]
#[test]
fn a_fired_precise_deadline_does_not_end_the_next_wait() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    let after = Duration::from_millis(1);
    poller.arm_precise(start + after, after);
    poller
        .wait(Some(Duration::from_secs(10)), |_, _| {})
        .unwrap();
    let again = Instant::now();
    assert!(
        !poller
            .wait(Some(Duration::from_millis(20)), |_, _| {})
            .unwrap()
    );
    assert!(again.elapsed() >= Duration::from_millis(20), "it timed out");
}
