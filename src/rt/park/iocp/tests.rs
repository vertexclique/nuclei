use std::os::windows::io::AsRawSocket;
use std::time::{Duration, Instant};

use super::{Poller, SOURCE};
use crate::syscore::windows::{
    DWORD, ERROR_IO_PENDING, HANDLE, OVERLAPPED, PostQueuedCompletionStatus, SOCKET, SOCKET_ERROR,
    WSABUF, WSAGetLastError, WSARecv,
};

#[test]
fn a_wait_times_out_without_a_wake() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    assert!(
        !poller
            .wait_at_most(64, Some(Duration::from_millis(20)), |_| {})
            .unwrap()
    );
    assert!(start.elapsed() >= Duration::from_millis(20), "never early");
}

/// A wait of a millisecond lasts about that, not the system's 15.6 ms
/// tick: the median of a run of them stays well under it.
#[test]
fn a_short_wait_is_not_rounded_up_to_the_system_tick() {
    let mut poller = Poller::new().unwrap();
    if poller.timer.is_none() {
        eprintln!("skipping: this system has no high-resolution timer");
        return;
    }
    let mut took: Vec<Duration> = (0..21)
        .map(|_| {
            let start = Instant::now();
            poller
                .wait_at_most(64, Some(Duration::from_millis(1)), |_| {})
                .unwrap();
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(1), "never early: {took:?}");
            took
        })
        .collect();
    took.sort();
    assert!(took[10] < Duration::from_millis(10), "{took:?}");
}

#[test]
fn a_zero_timeout_only_looks() {
    let mut poller = Poller::new().unwrap();
    let start = Instant::now();
    assert!(
        !poller
            .wait_at_most(64, Some(Duration::ZERO), |_| {})
            .unwrap()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_wake_before_the_wait_makes_it_return_at_once() {
    let mut poller = Poller::new().unwrap();
    let n = poller.notifier();
    n.notify();
    assert!(poller.wait_at_most(64, None, |_| {}).unwrap());
    // Nothing more is queued: the next wait times out.
    assert!(
        !poller
            .wait_at_most(64, Some(Duration::from_millis(1)), |_| {})
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
            .wait_at_most(64, Some(Duration::from_secs(10)), |_| {})
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

/// The packet of an operation reaches the callback with its `OVERLAPPED`
/// and its byte count; a wake beside it is told apart.
#[test]
fn an_operations_packet_reaches_the_callback_and_a_wake_is_told_apart() {
    let mut poller = Poller::new().unwrap();
    let mut ov = OVERLAPPED::zeroed();
    let ov_at: *mut OVERLAPPED = &mut ov;
    // SAFETY: a packet naming our own OVERLAPPED, which outlives the wait.
    assert_ne!(
        unsafe { PostQueuedCompletionStatus(poller.port.handle, 5, SOURCE, ov_at) },
        0
    );
    poller.notifier().notify();
    let mut seen = Vec::new();
    let woken = poller
        .wait_at_most(64, Some(Duration::from_secs(5)), |entry| {
            seen.push((entry.lpOverlapped, entry.dwNumberOfBytesTransferred));
        })
        .unwrap();
    assert!(woken);
    assert_eq!(seen, [(ov_at, 5)]);
}

/// Issues an overlapped receive on `socket` into `buf`, with `ov`.
fn receive(socket: SOCKET, buf: &mut [u8], ov: &mut OVERLAPPED) {
    let mut wsabuf = WSABUF {
        len: buf.len() as u32,
        buf: buf.as_mut_ptr(),
    };
    let mut flags: DWORD = 0;
    // SAFETY: the buffer and the OVERLAPPED outlive the operation, which
    // the caller waits for.
    let ret = unsafe {
        WSARecv(
            socket,
            &mut wsabuf,
            1,
            core::ptr::null_mut(),
            &mut flags,
            ov,
            core::ptr::null(),
        )
    };
    // SAFETY: reads this thread's last Winsock error.
    assert!(ret != SOCKET_ERROR || unsafe { WSAGetLastError() } == ERROR_IO_PENDING);
}

/// Waits on `poller` for the packet of `ov`, up to a deadline: whether it
/// came.
fn packet_of(poller: &mut Poller, ov: *mut OVERLAPPED, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let mut came = false;
    while !came && Instant::now() < deadline {
        poller
            .wait_at_most(64, Some(Duration::from_millis(20)), |entry| {
                came |= entry.lpOverlapped == ov;
            })
            .unwrap();
    }
    came
}

/// A socket bound to one port is moved to another (its shard went): the
/// next operation's packet comes to the new port, and none to the old.
#[test]
fn a_socket_bound_to_one_port_moves_its_completions_to_another() {
    let mut first = Poller::new().unwrap();
    let mut second = Poller::new().unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let raw = socket.as_raw_socket() as SOCKET;
    first.port.bind(raw as HANDLE).unwrap();
    let mut buf = [0u8; 16];
    let mut ov = OVERLAPPED::zeroed();
    receive(raw, &mut buf, &mut ov);
    peer.send_to(b"one", socket.local_addr().unwrap()).unwrap();
    assert!(packet_of(&mut first, &mut ov, Duration::from_secs(5)));
    second.port.bind(raw as HANDLE).unwrap();
    let mut ov = OVERLAPPED::zeroed();
    receive(raw, &mut buf, &mut ov);
    peer.send_to(b"two", socket.local_addr().unwrap()).unwrap();
    assert!(packet_of(&mut second, &mut ov, Duration::from_secs(5)));
    assert!(
        !packet_of(&mut first, &mut ov, Duration::from_millis(50)),
        "a packet still came to the old port"
    );
    assert_eq!(&buf[..3], b"two");
}
