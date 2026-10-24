//! The proactor facade: the backend, driving a thread's shard by hand, waking
//! it, and `drive`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use nuclei::{Handle, IoBackend, Proactor, drive};

#[test]
fn the_backend_is_the_platforms() {
    #[cfg(target_os = "linux")]
    assert_eq!(Proactor::backend(), IoBackend::Epoll);
    #[cfg(target_os = "macos")]
    assert_eq!(Proactor::backend(), IoBackend::Kqueue);
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
