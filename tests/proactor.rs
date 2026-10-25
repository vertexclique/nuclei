//! The proactor facade: the backend, driving a thread's shard by hand, waking
//! it, and `drive`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use nuclei::{Handle, IoBackend, Proactor, drive};

/// The backend reported is the one the shards run: io_uring or epoll on
/// Linux (as `NUCLEI_BACKEND` and the kernel decide), kqueue on macOS; and a
/// thread's own shard reports what it runs.
#[test]
fn the_backend_is_the_live_one() {
    let reported = Proactor::backend();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let named = std::env::var("NUCLEI_BACKEND").unwrap_or_default();
        match named.as_str() {
            "epoll" => assert_eq!(reported, IoBackend::Epoll),
            "io_uring" => assert_eq!(reported, IoBackend::IoUring),
            _ => assert!(matches!(reported, IoBackend::IoUring | IoBackend::Epoll)),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    assert_eq!(reported, IoBackend::Kqueue);
    assert_eq!(nuclei::block_on(async { Proactor::backend() }), reported);
}

/// A runtime built for the platform's poller runs it, whatever the default.
#[test]
fn a_runtime_built_for_the_poller_runs_it() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let poller = IoBackend::Epoll;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let poller = IoBackend::Kqueue;
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty()) {
        return;
    }
    let rt = nuclei::Builder::new()
        .worker_threads(1)
        .backend(poller)
        .build()
        .unwrap();
    let on_worker = rt.block_on(async { nuclei::spawn(async { Proactor::backend() }).await });
    assert_eq!(on_worker, poller);
}

/// A backend the platform has not got fails the build of a runtime, with
/// the reason.
#[test]
fn a_runtime_built_for_a_foreign_backend_fails() {
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty()) {
        return;
    }
    let Err(err) = nuclei::Builder::new()
        .worker_threads(1)
        .backend(IoBackend::Iocp)
        .build()
    else {
        panic!("a runtime on IOCP started on this platform");
    };
    assert!(err.to_string().contains("no Iocp backend"), "{err}");
}

#[test]
fn a_wait_with_nothing_to_do_lasts_its_duration() {
    let p = Proactor::get();
    let start = Instant::now();
    assert_eq!(p.wait(64, Some(Duration::from_millis(20))).unwrap(), 0);
    assert!(start.elapsed() >= Duration::from_millis(20));
}

#[test]
fn a_wait_handles_the_io_of_this_threads_shard() {
    let (mut peer, mine) = UnixStream::pair().unwrap();
    let handle = Handle::new(mine).unwrap();
    // A task of this thread's own shard: the waits below run it, and its
    // receive registers the socket with this shard.
    let task = nuclei::spawn_local(async move {
        let mut buf = [0u8; 4];
        let n = handle.recv(&mut buf).await.unwrap();
        (n, buf)
    });
    Proactor::get().wait(64, Some(Duration::ZERO)).unwrap();
    assert!(!task.is_finished(), "nothing to receive yet");
    peer.write_all(b"data").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = 0;
    while !task.is_finished() {
        assert!(Instant::now() < deadline, "the receive never completed");
        events += Proactor::get()
            .wait(64, Some(Duration::from_millis(100)))
            .unwrap();
    }
    assert!(events > 0, "the data came as an event");
    assert_eq!(nuclei::block_on(task), (4, *b"data"));
}

#[test]
fn drive_runs_a_future_to_its_end() {
    assert_eq!(drive(async { 3 }), 3);
}
