//! The owned stream writes at the zero-copy size and above: both ends that
//! write before they read finish, the zero-copy write's buffer comes back
//! through its `Released`, and writes pipeline.

use super::*;
use crate::net::Released;

const MIB: usize = 1 << 20;

/// A connected TCP pair, the far end accepted.
async fn tcp_pair() -> (Handle<TcpStream>, Handle<TcpStream>) {
    let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (near, far) = join(Handle::<TcpStream>::connect(addr), listener.accept()).await;
    let (near, far) = (near.unwrap(), far.unwrap().0);
    roomy(&near, true);
    roomy(&far, true);
    (near, far)
}

/// Gives `s` a send buffer of 4 MiB, and with `receive` a receive buffer too, on macOS, whose
/// TCP buffers stay at 128 KiB while nothing is read: a write that must fit before the peer
/// reads needs the room Linux's buffers grow to. Elsewhere the system's own sizing stays (a
/// size set on Linux caps the buffer and stops it growing).
#[cfg(target_vendor = "apple")]
fn roomy(s: &Handle<TcpStream>, receive: bool) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 4 << 20;
    let options = if receive {
        &[libc::SO_SNDBUF, libc::SO_RCVBUF][..]
    } else {
        &[libc::SO_SNDBUF][..]
    };
    for &option in options {
        // SAFETY: a plain setsockopt with a pointer to our own int.
        let set = unsafe {
            libc::setsockopt(
                s.get_ref().as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&size as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0, "{}", io::Error::last_os_error());
    }
}

#[cfg(not(target_vendor = "apple"))]
fn roomy(_: &Handle<TcpStream>, _: bool) {}

/// Reads `len` bytes into one buffer, with owned reads.
async fn read_len(s: &Handle<TcpStream>, len: usize) -> Vec<u8> {
    let mut got = Vec::with_capacity(len);
    while got.len() < len {
        let (n, back) = s.read_owned(got).await;
        assert!(n.unwrap() > 0, "the stream ended early");
        got = back;
    }
    got
}

/// Both ends write `len` bytes with `write` and then read the other's: each
/// gets the other's bytes, and then its own buffer back, within the deadline.
fn exchange<W, F>(len: usize, write: W)
where
    W: Fn(Handle<TcpStream>, Vec<u8>) -> F + Copy + Send + 'static,
    F: Future<Output = (Handle<TcpStream>, Released)> + Send,
{
    let rt = runtime();
    rt.block_on(async move {
        let (a, b) = tcp_pair().await;
        let side = move |s: Handle<TcpStream>, seed: usize| async move {
            let data: Vec<u8> = pattern(len + seed)[seed..].to_vec();
            let (s, released) = write(s, data.clone()).await;
            let got = read_len(&s, len).await;
            assert!(within(released).await == data, "the buffer changed");
            got
        };
        let far = spawn(side(b, 1));
        let near = side(a, 0);
        let (got_near, got_far) = within(join(near, far)).await;
        assert!(
            got_near == pattern(len + 1)[1..],
            "the near end got other bytes"
        );
        assert!(got_far == pattern(len), "the far end got other bytes");
    });
}

#[test]
fn both_ends_write_owned_and_then_read() {
    for len in [MIB, 2 * MIB] {
        exchange(len, |s, buf| async move {
            let (n, back) = s.write_owned(buf).await;
            assert_eq!(n.unwrap(), back.len());
            (s, Released::ready(crate::buf::owned::OwnedBuf::Vec(back)))
        });
    }
}

#[test]
fn both_ends_write_zero_copy_and_then_read() {
    for len in [MIB, 2 * MIB] {
        exchange(len, |s, buf| async move {
            let want = buf.len();
            let (n, released) = s.write_zero_copy(buf).await;
            assert_eq!(n.unwrap(), want);
            (s, released)
        });
    }
}

/// Whether the calling task runs on a readiness poller's shard.
fn on_poller() -> bool {
    #[cfg(ring)]
    return crate::rt::context::current(|shard| shard.is_some_and(|s| s.ring().is_none()));
    #[cfg(not(ring))]
    true
}

/// On a driver without zero-copy sends the write is copied, and its buffer is back at once.
#[test]
fn a_zero_copy_write_on_a_poller_is_copied_and_ready_at_once() {
    let poller = if cfg!(windows) {
        "iocp"
    } else if cfg!(target_os = "linux") {
        "epoll"
    } else {
        "kqueue"
    };
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty() && b != "auto" && b != poller) {
        eprintln!("skipping: NUCLEI_BACKEND names another backend than the poller");
        return;
    }
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .backend(crate::proactor::POLLER)
        .build()
        .unwrap();
    // On the worker: the thread calling `block_on` runs a shard of its own, of any backend, and
    // takes what is spawned before the worker has started.
    rt.block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut pause = Duration::from_millis(1);
        while !within(spawn(async { on_poller() })).await {
            assert!(
                std::time::Instant::now() < deadline,
                "the poller worker never started"
            );
            crate::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(50));
        }
        within(spawn(async {
            assert!(on_poller(), "a task moved off the started worker");
            let (a, b) = tcp_pair().await;
            let data = pattern(2 * MIB);
            let at = data.as_ptr().addr();
            let read = spawn(async move { read_len(&b, 2 * MIB).await });
            let (n, mut released) = within(a.write_zero_copy(data)).await;
            assert_eq!(n.unwrap(), 2 * MIB);
            let polled = Pin::new(&mut released).poll(&mut core::task::Context::from_waker(
                core::task::Waker::noop(),
            ));
            let Poll::Ready(back) = polled else {
                panic!("the copied write's buffer did not come back at once");
            };
            assert_eq!(back.as_ptr().addr(), at);
            assert!(within(read).await == back);
        }))
        .await
    });
}

/// The buffers come back on a thread outside the runtime, whose workers run the IO.
#[test]
fn released_buffers_come_back_on_any_thread() {
    let rt = runtime();
    let (released, sent) = rt.block_on(async {
        let (a, b) = tcp_pair().await;
        let bufs: Vec<Vec<u8>> = (0..4).map(|i| pattern(MIB + i)).collect();
        let total = bufs.iter().map(Vec::len).sum();
        let read = spawn(async move { read_len(&b, total).await });
        let mut released = Vec::new();
        for buf in &bufs {
            let (n, back) = within(a.write_zero_copy(buf.clone())).await;
            assert_eq!(n.unwrap(), buf.len());
            released.push(back);
        }
        assert!(
            within(read).await == bufs.concat(),
            "the writes arrived out of order"
        );
        (released, bufs)
    });
    let back =
        std::thread::spawn(move || released.into_iter().map(plain_block_on).collect::<Vec<_>>());
    assert!(back.join().unwrap() == sent);
}

/// A listener whose connections hold 64 KiB of what they receive: what a write sends past that
/// waits, unacknowledged, until the peer reads.
#[cfg(unix)]
fn held_back_listener() -> TcpListener {
    use std::os::fd::AsRawFd;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let small: libc::c_int = 64 * 1024;
    // SAFETY: a plain setsockopt with a pointer to our own int; accepted sockets inherit it.
    let set = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&small as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    assert_eq!(set, 0);
    listener
}

/// A runtime shut down while a worker's send waits for its notification, the peer reading only
/// after: the shutdown does not wait for the peer, and the buffer comes back once it read.
#[cfg(unix)]
#[test]
fn a_buffer_comes_back_after_its_runtime_shut_down() {
    let listener = held_back_listener();
    let addr = listener.local_addr().unwrap();
    let data = pattern(MIB);
    let sent = data.clone();
    let rt = runtime();
    let (stream, mut released, zero_copy) = rt.block_on(async move {
        within(spawn(async move {
            let s = Handle::<TcpStream>::connect(addr).await.unwrap();
            roomy(&s, false);
            #[cfg(ring)]
            let zero_copy = crate::rt::context::current(|shard| {
                shard
                    .and_then(|s| s.ring())
                    .is_some_and(crate::rt::shard::ring::ShardRing::zc_stream)
            });
            #[cfg(not(ring))]
            let zero_copy = false;
            let (n, released) = s.write_zero_copy(sent).await;
            assert_eq!(n.unwrap(), MIB);
            (s, released, zero_copy)
        }))
        .await
    });
    let (mut peer, _) = listener.accept().unwrap();
    let early = match Pin::new(&mut released).poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    )) {
        Poll::Ready(back) => Some(back),
        Poll::Pending => None,
    };
    assert!(
        early.is_none() || !zero_copy,
        "the buffer came back before the peer read it"
    );
    drop(rt);
    let mut got = vec![0u8; MIB];
    peer.read_exact(&mut got).unwrap();
    assert!(got == data, "the bytes changed on the way");
    let back = early.unwrap_or_else(|| plain_block_on(released));
    assert!(back == data, "the buffer changed");
    drop(stream);
}

/// A thread's own shard runs a ring only that thread may reap: its zero-copy write is copied,
/// so `block_on` has nothing to wait for and the buffer is back at once.
#[cfg(unix)]
#[test]
fn a_zero_copy_write_on_a_threads_own_shard_is_copied() {
    let listener = held_back_listener();
    let addr = listener.local_addr().unwrap();
    let data = pattern(MIB);
    let sent = data.clone();
    let (stream, mut released) = crate::block_on(async move {
        let s = within(Handle::<TcpStream>::connect(addr)).await.unwrap();
        roomy(&s, false);
        let (n, released) = within(s.write_zero_copy(sent)).await;
        assert_eq!(n.unwrap(), MIB);
        (s, released)
    });
    let Poll::Ready(back) = Pin::new(&mut released).poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    )) else {
        panic!("the copied write's buffer did not come back at once");
    };
    assert!(back == data);
    let (mut peer, _) = listener.accept().unwrap();
    let mut got = vec![0u8; MIB];
    peer.read_exact(&mut got).unwrap();
    assert!(got == data);
    drop(stream);
}
