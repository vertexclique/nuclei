//! Each counter moves for its event, on a shard made for the test (nothing
//! else runs on it), read from a future that shard runs.

use core::future::{Future, poll_fn};
use core::task::Poll;
use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::snapshot;
use crate::IoBackend;
use crate::rt::backend::{self, Backend};
use crate::rt::context;
use crate::rt::shard::{Limits, Shard};
use crate::stats::ShardStats;
use crate::{Handle, spawn_local};

/// Runs `fut` on a new shard of `backend` whose pool holds `buffer_cap`
/// bytes, entered on this thread.
fn run_on<T>(backend: Backend, buffer_cap: usize, fut: impl Future<Output = T>) -> T {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap,
        backend,
    })
    .unwrap();
    let _current = context::enter(&shard);
    shard.block_on(fut)
}

/// Runs `fut` on a new shard of the process's backend.
fn run<T>(fut: impl Future<Output = T>) -> T {
    run_on(backend::chosen().unwrap(), crate::buf::DEFAULT_CAP, fut)
}

/// The counters of the shard running the caller.
fn here() -> ShardStats {
    context::current(|shard| shard.expect("a shard runs this").shared.stats())
}

/// Makes the shard running the caller publish what its own cells hold.
fn publish() {
    context::current(|shard| shard.expect("a shard runs this").publish_now());
}

/// Yields, letting the shard run a task, until what it publishes then
/// meets `cond`; a few hundred yields at most.
async fn published_until(what: &str, cond: impl Fn(&ShardStats) -> bool) {
    for _ in 0..256 {
        yield_now().await;
        publish();
        if cond(&here()) {
            return;
        }
    }
    panic!("never {what}: {:?}", here());
}

async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

/// A connected pair: the std end, and the accepted end as a handle.
fn tcp_pair() -> (TcpStream, Handle<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, Handle::new(server).unwrap())
}

/// An address nothing listens on.
fn closed_addr() -> std::net::SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[test]
fn polls_count_every_poll_of_a_task_and_tasks_the_live_ones() {
    run(async {
        let start = here();
        assert_eq!((start.worker, start.tasks, start.polls), (None, 0, 0));
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                spawn_local(async {
                    for _ in 0..4 {
                        yield_now().await;
                    }
                })
            })
            .collect();
        assert_eq!(here().tasks, 10);
        for task in tasks {
            task.await;
        }
        publish();
        let end = here();
        assert_eq!(end.tasks, 0);
        assert_eq!(
            end.polls, 50,
            "five polls of each: four yields, then the end"
        );
    });
}

#[test]
fn a_task_woken_from_another_thread_is_one_remote_wake() {
    run(async {
        let (tx, rx) = crate::sync::oneshot::channel::<u8>();
        let task = spawn_local(rx);
        // The task runs, and waits.
        yield_now().await;
        publish();
        let before = here().remote_wakes;
        std::thread::spawn(move || tx.send(7).unwrap())
            .join()
            .unwrap();
        assert_eq!(task.await.unwrap(), 7);
        publish();
        assert_eq!(here().remote_wakes, before + 1);
    });
}

#[test]
fn timers_count_the_armed_ones() {
    run(async {
        let sleepers: Vec<_> = (0..5)
            .map(|_| spawn_local(crate::time::sleep(Duration::from_secs(60))))
            .collect();
        // The loop runs a task between polls of this future: the five arm
        // their timers over a few yields.
        published_until("the five armed", |s| s.timers == 5).await;
        // Dropped, they are cancelled as they run once more.
        drop(sleepers);
        published_until("none armed", |s| s.timers == 0).await;
    });
}

#[test]
fn the_buffer_pool_counts_its_bytes_and_each_time_it_ran_dry() {
    let rt = crate::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .io_buffer_cap(0)
        .build()
        .unwrap();
    let task = rt.spawn(async {
        let start = here();
        assert_eq!(start.worker, Some(0));
        let smallest = crate::buf::CLASSES[0];
        assert_eq!(
            start.buffer_cap, smallest,
            "the pool holds one buffer at least"
        );
        let held = crate::buf::with_pool(|pool| {
            let pool = pool.expect("a worker's pool");
            let held = pool.get(smallest).expect("the one buffer");
            assert!(pool.get(smallest).is_none(), "no room for a second");
            held
        });
        publish();
        let dry = here();
        assert_eq!((dry.buffer_bytes, dry.buffer_exhausted), (smallest, 1));
        drop(held);
        publish();
        // Free, and still held by the pool for the next read.
        assert_eq!(here().buffer_bytes, smallest);
    });
    rt.block_on(task);
}

#[test]
fn ops_in_flight_count_what_the_kernel_holds() {
    run(async {
        let (mut client, server) = tcp_pair();
        let reader = spawn_local(async move {
            let mut buf = [0u8; 8];
            server.recv(&mut buf).await
        });
        // The reader runs, and waits with its receive out.
        yield_now().await;
        publish();
        let waiting = here();
        assert_eq!(
            waiting.backend,
            context::current(|s| s.unwrap().io_backend())
        );
        match waiting.backend {
            // The driver makes the call once the socket is ready.
            IoBackend::Epoll | IoBackend::Kqueue => assert_eq!(waiting.ops_in_flight, 0),
            IoBackend::IoUring | IoBackend::Iocp => assert!(
                waiting.ops_in_flight >= 1,
                "a receive in the kernel: {waiting:?}"
            ),
        }
        client.write_all(b"x").unwrap();
        assert_eq!(reader.await.unwrap(), 1);
    });
}

#[test]
fn an_error_counts_its_kind_against_the_shard_that_met_it() {
    run(async {
        let before = here().errors;
        let refused = Handle::<TcpStream>::connect(closed_addr())
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
        let missing = Handle::<File>::open("/nonexistent/nuclei/stats")
            .await
            .unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        let after = here().errors;
        assert_eq!(
            after.get(ErrorKind::ConnectionRefused),
            before.get(ErrorKind::ConnectionRefused) + 1
        );
        assert_eq!(
            after.get(ErrorKind::NotFound),
            before.get(ErrorKind::NotFound) + 1
        );
        let kinds: Vec<_> = after.iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [ErrorKind::NotFound, ErrorKind::ConnectionRefused]);
    });
}

/// A file's read or write that fails (on a file not opened for it) counts
/// its kind, on the pool and on a ring alike.
#[test]
fn a_file_call_that_fails_counts_its_kind() {
    let path = std::env::temp_dir().join(format!("nuclei-stats-{}", std::process::id()));
    std::fs::write(&path, b"some bytes").unwrap();
    run(async {
        let write_only = Handle::new(File::options().write(true).open(&path).unwrap()).unwrap();
        let read_only = Handle::new(File::open(&path).unwrap()).unwrap();
        let before = here().errors;
        let (read, _) = write_only.read_owned(Vec::with_capacity(8)).await;
        let read = read.unwrap_err();
        let (written, _) = read_only.write_owned(b"more".to_vec()).await;
        let written = written.unwrap_err();
        let after = here().errors;
        if read.kind() == written.kind() {
            assert_eq!(after.get(read.kind()), before.get(read.kind()) + 2);
        } else {
            assert_eq!(after.get(read.kind()), before.get(read.kind()) + 1);
            assert_eq!(after.get(written.kind()), before.get(written.kind()) + 1);
        }
    });
    std::fs::remove_file(&path).unwrap();
}

/// A unix listener at a fresh path in `dir` whose backlog is full, and
/// the connections that fill it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn full_backlog(
    dir: &std::path::Path,
) -> (
    std::os::unix::net::UnixListener,
    Vec<std::os::fd::OwnedFd>,
    std::path::PathBuf,
) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir(dir).unwrap();
    let path = dir.join("sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // A backlog of none: one connection waits, and the next finds it full.
    // SAFETY: a plain call on the listener's own descriptor.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let mut waiting = Vec::new();
    loop {
        // SAFETY: a plain call; the descriptor is owned at once.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0) };
        assert!(fd >= 0);
        // SAFETY: just made, and ours.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        match nonblocking_connect(&sock, &path) {
            Ok(()) => waiting.push(sock),
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => break,
            Err(err) => panic!("{err}"),
        }
    }
    (listener, waiting, path)
}

/// A unix connect that finds the listener's backlog full is made again as
/// a blocking connect: its "try again" is no error.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_connect_made_again_another_way_counts_no_error() {
    let dir = std::env::temp_dir().join(format!("nuclei-stats-unix-{}", std::process::id()));
    let (listener, waiting, path) = full_backlog(&dir);
    // Once nuclei's connect waits in its blocking call, the backlog empties.
    let acceptor = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        (0..=waiting.len())
            .map(|_| listener.accept().unwrap().0)
            .collect::<Vec<_>>()
    });
    run(async {
        let before = here().errors;
        let stream = Handle::<std::os::unix::net::UnixStream>::connect(&path)
            .await
            .unwrap();
        assert_eq!(here().errors, before);
        drop(stream);
    });
    drop(acceptor.join().unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The blocking connect made again fails as its listener closes: that is
/// an error, counted.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_connect_made_again_that_fails_counts_its_error() {
    let dir = std::env::temp_dir().join(format!("nuclei-stats-unix-gone-{}", std::process::id()));
    let (listener, waiting, path) = full_backlog(&dir);
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        drop((listener, waiting));
    });
    run(async {
        let before = here().errors;
        let err = Handle::<std::os::unix::net::UnixStream>::connect(&path)
            .await
            .unwrap_err();
        assert_eq!(
            here().errors.get(err.kind()),
            before.get(err.kind()) + 1,
            "{err}"
        );
    });
    closer.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn nonblocking_connect(sock: &std::os::fd::OwnedFd, path: &std::path::Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: plain data, all zeroes valid.
    let mut addr: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
        *dst = *src as libc::c_char;
    }
    // SAFETY: the address is a whole `sockaddr_un`, the length its own.
    let r = unsafe {
        libc::connect(
            sock.as_raw_fd(),
            core::ptr::from_ref(&addr).cast(),
            core::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A datagram to a port nothing listens on comes back as a refusal, which
/// the next receive of the connected socket reports.
#[cfg(unix)]
#[test]
fn a_receive_that_fails_counts_its_kind() {
    use std::net::UdpSocket;

    run(async {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let gone = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(gone.local_addr().unwrap()).unwrap();
        drop(gone);
        let socket = Handle::new(socket).unwrap();
        let before = here().errors.get(ErrorKind::ConnectionRefused);
        socket.send(b"anyone?").await.unwrap();
        let mut buf = [0u8; 8];
        let err = crate::time::timeout(Duration::from_secs(10), socket.recv(&mut buf))
            .await
            .expect("the refusal came")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
        assert_eq!(here().errors.get(ErrorKind::ConnectionRefused), before + 1);
    });
}

#[test]
fn a_try_again_is_no_error() {
    run(async {
        let before = here().errors;
        let (mut client, server) = tcp_pair();
        let reader = spawn_local(async move {
            let mut buf = [0u8; 8];
            server.recv(&mut buf).await
        });
        // The reader runs first, and finds nothing: a try again.
        yield_now().await;
        client.write_all(b"x").unwrap();
        assert_eq!(reader.await.unwrap(), 1);
        assert_eq!(here().errors, before);
    });
}

/// IO on a thread that runs no shard counts in the process's table.
#[test]
fn an_error_off_any_shard_counts_in_the_process_table() {
    let before = snapshot().off_shard_errors.get(ErrorKind::NotFound);
    let missing = std::thread::spawn(|| {
        crate::sync::internal::block_on(Handle::<File>::open("/nonexistent/nuclei/off"))
    })
    .join()
    .unwrap()
    .unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    // Other tests may count there too.
    assert!(snapshot().off_shard_errors.get(ErrorKind::NotFound) > before);
}

/// epoll refuses a descriptor that cannot be polled (`/dev/null`). Such a
/// descriptor never makes a call wait, so the read is made to: the pool's
/// only buffer is held, and the read waits for one, watched by the driver.
#[cfg(target_os = "linux")]
#[test]
fn a_registration_the_kernel_refuses_is_counted() {
    run_on(Backend::Poll, 0, async {
        let held = crate::buf::with_pool(|pool| pool.unwrap().get(1).unwrap());
        let devnull = File::open("/dev/null").unwrap();
        let stream = Handle::new(std::os::unix::net::UnixStream::from(
            std::os::fd::OwnedFd::from(devnull),
        ))
        .unwrap();
        let mut buf = [0u8; 8];
        let read = stream.recv(&mut buf).await;
        assert_eq!(read.unwrap_err().raw_os_error(), Some(libc::EPERM));
        assert_eq!(here().errors.refused_registrations, 1);
        drop(held);
    });
}

/// An accept with no descriptor left fails, and is counted as one for want
/// of descriptors. Alone in its process: it lowers the descriptor limit. On
/// a ring, with the listener in the fixed-file table and on a copy (as on
/// kernels before 6.13): the registration, which may make a copy, happens
/// while descriptors are left, so what fails is the accept.
#[cfg(unix)]
#[test]
fn an_accept_with_no_descriptor_left_is_counted() {
    if !crate::alone::alone("rt::counters::tests::an_accept_with_no_descriptor_left_is_counted") {
        return;
    }
    for table in [None, Some(0)] {
        #[cfg(ring)]
        crate::rt::shard::ring::TABLE_FOR_TEST.with(|t| t.set(table));
        #[cfg(not(ring))]
        let _ = table;
        accept_with_no_descriptor_left();
    }
}

#[cfg(unix)]
fn accept_with_no_descriptor_left() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    run(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
        let addr = listener.get_ref().local_addr().unwrap();
        // The client's socket is made while one can be.
        // SAFETY: a plain call; the descriptor is owned at once.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);
        // SAFETY: just made, and ours.
        let client = unsafe { OwnedFd::from_raw_fd(fd) };
        // The accept waits, its listener registered. On a ring its entry
        // is only queued: the kernel takes the descriptor limit as it takes
        // the entry in, at the shard's next wait, after the limit is
        // lowered below (no await comes between).
        let mut accept = core::pin::pin!(listener.accept());
        poll_fn(|cx| {
            assert!(accept.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // SAFETY: plain data, all zeroes valid; the kernel fills it.
        let mut limit: libc::rlimit = unsafe { core::mem::zeroed() };
        // SAFETY: plain calls on this process's own limit and descriptors.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            let lowest_free = libc::dup(client.as_raw_fd());
            assert!(lowest_free >= 0);
            libc::close(lowest_free);
            let tight = libc::rlimit {
                rlim_cur: lowest_free as libc::rlim_t,
                rlim_max: limit.rlim_max,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &tight), 0);
        }
        let connected = connect_inet(&client, addr);
        let accepted = crate::time::timeout(Duration::from_secs(10), accept).await;
        // SAFETY: as above.
        unsafe { assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0) };
        connected.unwrap();
        let err = accepted.expect("the accept ended").unwrap_err();
        assert!(
            matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)),
            "{err}"
        );
        let errors = here().errors;
        assert_eq!(errors.refused_registrations, 0, "{errors:?}");
        assert_eq!(errors.accepts_out_of_descriptors, 1, "{errors:?}");
        assert!(errors.get(ErrorKind::Other) >= 1, "{errors:?}");
    });
}

/// A blocking connect of `sock` to `addr` (an IPv4 one).
#[cfg(unix)]
fn connect_inet(sock: &std::os::fd::OwnedFd, addr: std::net::SocketAddr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let std::net::SocketAddr::V4(v4) = addr else {
        panic!("an IPv4 address");
    };
    // SAFETY: plain data, all zeroes valid.
    let mut raw: libc::sockaddr_in = unsafe { core::mem::zeroed() };
    raw.sin_family = libc::AF_INET as libc::sa_family_t;
    raw.sin_port = v4.port().to_be();
    raw.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
    // SAFETY: the address is a whole `sockaddr_in`, the length its own.
    let r = unsafe {
        libc::connect(
            sock.as_raw_fd(),
            core::ptr::from_ref(&raw).cast(),
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether the registry has an entry for the shard of `Shared` at `key`.
fn entered(key: usize) -> bool {
    super::registry().get(&key).is_some()
}

#[test]
fn a_snapshot_lists_a_shard_from_its_making_to_its_drop() {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: backend::chosen().unwrap(),
    })
    .unwrap();
    let key = std::sync::Arc::as_ptr(&shard.shared) as usize;
    assert!(entered(key));
    // A waker may keep the `Shared` past the shard.
    let kept = std::sync::Arc::clone(&shard.shared);
    drop(shard);
    assert!(kept.counters.is_gone());
    snapshot();
    assert!(!entered(key), "taken out by the snapshot after it went");
    drop(kept);
}

/// A thread's own shard goes in the thread's exit; the next snapshot takes
/// its entry out.
#[test]
fn a_shard_gone_with_its_thread_is_taken_out() {
    // The weak reference keeps the address from another shard meanwhile.
    let shared = std::thread::spawn(|| {
        crate::block_on(async {
            context::current(|s| std::sync::Arc::downgrade(&s.unwrap().shared))
        })
    })
    .join()
    .unwrap();
    snapshot();
    assert!(!entered(shared.as_ptr() as usize));
}

#[test]
fn a_kind_outside_the_table_counts_as_other() {
    assert_eq!(super::kind_index(ErrorKind::Other), super::KINDS.len() - 1);
    let other = io::Error::from_raw_os_error(i32::MAX);
    assert_eq!(super::kind_index(other.kind()), super::KINDS.len() - 1);
    for (i, kind) in super::KINDS.iter().enumerate() {
        assert_eq!(super::kind_index(*kind), i, "{kind:?} is counted apart");
    }
}
