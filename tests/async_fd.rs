//! A caller's own descriptor driven by readiness: the waits, the guards'
//! calls and clears, the interest it is watched for, and every byte of a
//! peer that writes while the caller reads, on each backend.
#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use nuclei::io::Interest;
use nuclei::io::unix::AsyncFd;

fn within<F: core::future::Future>(f: F) -> F::Output {
    nuclei::block_on(async {
        nuclei::time::timeout(Duration::from_secs(30), f)
            .await
            .expect("nothing came within thirty seconds")
    })
}

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    (a, b)
}

#[test]
fn a_guard_runs_the_callers_read_and_clears_when_it_would_block() {
    within(async {
        let (a, mut b) = pair();
        let fd = AsyncFd::new(a).unwrap();
        let mut buf = [0u8; 16];
        // The side starts ready; the read would block, and the guard clears.
        let mut guard = fd.readable().await.unwrap();
        assert!(guard.ready().is_readable());
        assert!(guard.try_io(|fd| fd.get_ref().read(&mut buf)).is_err());
        let err = fd
            .try_io(Interest::READABLE, |mut a| a.read(&mut buf))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock, "cleared, so not run");
        b.write_all(b"hello").unwrap();
        let mut guard = fd.readable().await.unwrap();
        let n = guard
            .try_io(|fd| fd.get_ref().read(&mut buf))
            .expect("the bytes came")
            .unwrap();
        assert_eq!(&buf[..n], b"hello");
    });
}

#[test]
fn a_full_socket_waits_until_the_peer_drains_it() {
    within(async {
        let (a, mut b) = pair();
        let fd = AsyncFd::new(a).unwrap();
        let chunk = [9u8; 4096];
        let mut sent = 0usize;
        // Fill the socket until a write would block; the guard clears.
        loop {
            let mut guard = fd.writable().await.unwrap();
            match guard.try_io(|fd| fd.get_ref().write(&chunk)) {
                Ok(n) => sent += n.unwrap(),
                Err(_) => break,
            }
        }
        assert!(sent > 0);
        let drain = std::thread::spawn(move || {
            let mut buf = vec![0u8; sent];
            b.read_exact(&mut buf).unwrap();
            b
        });
        let n = fd
            .async_io(Interest::WRITABLE, |mut a| a.write(b"after"))
            .await
            .unwrap();
        assert!(n > 0);
        let mut b = drain.join().unwrap();
        let mut tail = [0u8; 5];
        b.read_exact(&mut tail[..n]).unwrap();
        assert_eq!(&tail[..n], &b"after"[..n]);
    });
}

#[test]
fn every_byte_of_a_busy_peer_is_read() {
    const BYTES: usize = 200_000;
    within(async {
        let (a, mut b) = pair();
        let fd = AsyncFd::with_interest(a, Interest::READABLE).unwrap();
        let writer = std::thread::spawn(move || {
            // Small writes, each an edge of its own, racing the reads.
            for chunk in [7u8; BYTES].chunks(97) {
                b.write_all(chunk).unwrap();
            }
        });
        let mut buf = [0u8; 1000];
        let mut got = 0;
        while got < BYTES {
            let n = fd
                .async_io(Interest::READABLE, |mut a| a.read(&mut buf))
                .await
                .unwrap();
            assert!(n > 0, "the peer ended early, at {got}");
            assert!(buf[..n].iter().all(|&x| x == 7));
            got += n;
        }
        writer.join().unwrap();
        assert_eq!(got, BYTES);
    });
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn an_eventfd_counts_what_another_thread_adds() {
    use std::os::fd::{FromRawFd, OwnedFd};
    const ADDS: u64 = 10_000;
    within(async {
        // SAFETY: a plain call; the descriptor it returns is owned here.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: `raw` is a descriptor of this process nothing else owns.
        let event = std::sync::Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        let adder = std::sync::Arc::clone(&event);
        let fd = AsyncFd::with_interest(event, Interest::READABLE).unwrap();
        let thread = std::thread::spawn(move || {
            let mut file = std::fs::File::from(adder.try_clone().unwrap());
            for _ in 0..ADDS {
                file.write_all(&1u64.to_ne_bytes()).unwrap();
            }
        });
        let mut total = 0;
        while total < ADDS {
            let mut value = [0u8; 8];
            fd.async_io(Interest::READABLE, |event| {
                let mut file = std::fs::File::from(event.try_clone()?);
                file.read_exact(&mut value)
            })
            .await
            .unwrap();
            total += u64::from_ne_bytes(value);
        }
        thread.join().unwrap();
        assert_eq!(total, ADDS);
    });
}

#[test]
fn a_wait_for_what_the_descriptor_is_not_watched_for_fails() {
    within(async {
        let (a, _b) = pair();
        let fd = AsyncFd::with_interest(a, Interest::READABLE).unwrap();
        let err = fd.writable().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = fd
            .try_io(Interest::WRITABLE, |mut a| a.write(b"x"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Either of two is enough.
        let guard = fd
            .ready(Interest::READABLE | Interest::WRITABLE)
            .await
            .unwrap();
        assert!(guard.ready().is_readable() && !guard.ready().is_writable());
    });
}

#[test]
fn a_mutable_guard_reaches_the_descriptor_and_it_comes_back() {
    within(async {
        let (a, mut b) = pair();
        let mut fd = AsyncFd::new(a).unwrap();
        b.write_all(b"mut").unwrap();
        let mut buf = [0u8; 3];
        loop {
            let mut guard = fd.readable_mut().await.unwrap();
            match guard.try_io(|fd| fd.get_mut().read(&mut buf)) {
                Ok(n) => {
                    assert_eq!(n.unwrap(), 3);
                    break;
                }
                Err(_) => continue,
            }
        }
        assert_eq!(&buf, b"mut");
        let mut back = fd.into_inner();
        back.write_all(b"ok").unwrap();
        let mut two = [0u8; 2];
        b.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"ok");
    });
}
