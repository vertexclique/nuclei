//! Reads and writes that lend the side the task's own buffer: the driver
//! reads into it and writes from it with no pool buffer, and a future
//! dropped while its buffer is lent loses no byte and tears no write.

use super::*;
use crate::io::Handle;

/// What this thread's pool took from the allocator.
fn pool_charged() -> usize {
    crate::buf::with_pool(|p| p.map_or(0, |p| p.charged()))
}

/// Runs `fut` as a task on the runtime's worker, whose driver serves the
/// sources it polls and whose pool any copy would take a buffer from.
fn on_worker<F>(rt: &Runtime, fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Longer than the steps' own deadlines, so a step that never ends says
    // so itself.
    rt.block_on(async move {
        crate::time::timeout(Duration::from_secs(30), crate::runtime::spawn(fut))
            .await
            .expect("the task never completed")
    })
}

/// Waits, letting the shard run, until `done` holds; at its deadline it
/// fails with `report`.
async fn until_or(mut done: impl FnMut() -> bool, report: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "it never happened: {}", report());
        crate::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// An empty buffer of exactly `cap` bytes.
fn exactly(cap: usize) -> Vec<u8> {
    let mut buf = vec![0u8; cap].into_boxed_slice().into_vec();
    buf.clear();
    buf
}

/// Reads `src` to its end with lent buffers.
async fn read_to_end(src: &Arc<Source>) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        let (n, back) = src.read_owned(buf).await;
        buf = back;
        if n.unwrap() == 0 {
            return all;
        }
        all.extend_from_slice(&buf);
        buf.clear();
    }
}

#[test]
fn a_lent_read_takes_ready_bytes_after_what_the_buffer_holds() {
    let rt = runtime(None);
    let (a, b) = pair();
    a.io().write_all(b"hello").unwrap();
    let src = Arc::clone(&b.src);
    let (n, buf, charged) = on_worker(&rt, async move {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(b"> ");
        let (n, buf) = src.read_owned(buf).await;
        (n.unwrap(), buf, pool_charged())
    });
    assert_eq!(n, 5);
    assert_eq!(buf, b"> hello");
    assert_eq!(charged, 0, "the bytes went through a pool buffer");
}

#[test]
fn a_lent_read_that_waits_is_filled_by_the_driver() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src, peer) = (Arc::clone(&b.src), a.io().try_clone().unwrap());
    let (n, buf, charged) = on_worker(&rt, async move {
        let mut read = Box::pin(src.read_owned(Vec::with_capacity(64)));
        pends(&mut read).await;
        send(&peer, b"late");
        let (n, buf) = read.await;
        (n.unwrap(), buf, pool_charged())
    });
    assert_eq!((n, &buf[..]), (4, &b"late"[..]));
    assert_eq!(charged, 0, "the driver read into a pool buffer");
    drop(a);
}

#[test]
fn a_lent_read_without_spare_capacity_reads_nothing() {
    let rt = runtime(None);
    let (a, b) = pair();
    a.io().write_all(b"x").unwrap();
    rt.block_on(async {
        let full = vec![1u8, 2, 3].into_boxed_slice().into_vec();
        let (n, back) = b.src.read_owned(full).await;
        assert_eq!(n.unwrap(), 0);
        assert_eq!(back, [1, 2, 3]);
        let mut buf = [0u8; 4];
        assert_eq!(read(&b.src, &mut buf).await.unwrap(), 1, "the byte stayed");
    });
}

#[test]
fn a_lent_read_at_the_end_of_the_stream_reads_zero() {
    let rt = runtime(None);
    let (a, b) = pair();
    drop(a);
    rt.block_on(async {
        let (n, back) = within(b.src.read_owned(Vec::with_capacity(8))).await;
        assert_eq!(n.unwrap(), 0);
        assert!(back.is_empty());
    });
}

#[test]
fn bytes_read_ahead_go_into_a_lent_buffer_first() {
    let rt = runtime(None);
    let (a, b) = pair();
    a.io().write_all(b"abcdef").unwrap();
    rt.block_on(async {
        let mut two = [0u8; 2];
        assert_eq!(read(&b.src, &mut two).await.unwrap(), 2);
        assert_eq!(&two, b"ab");
        let (n, buf) = b.src.read_owned(exactly(3)).await;
        assert_eq!((n.unwrap(), &buf[..]), (3, &b"cde"[..]));
        let (n, buf) = b.src.read_owned(Vec::with_capacity(64)).await;
        assert_eq!((n.unwrap(), &buf[..]), (1, &b"f"[..]));
        a.io().write_all(b"gh").unwrap();
        let (n, buf) = within(b.src.read_owned(Vec::with_capacity(64))).await;
        assert_eq!((n.unwrap(), &buf[..]), (2, &b"gh"[..]));
    });
}

/// The driver fills a lent buffer, and its future is dropped before it
/// takes it: those bytes come first to the next read, before the ones the
/// kernel still holds, and a read meanwhile waits rather than take those.
#[test]
fn a_dropped_lent_read_hands_its_bytes_to_the_next_read() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src, peer) = (Arc::clone(&b.src), a.io().try_clone().unwrap());
    let (first, rest) = on_worker(&rt, async move {
        let woke = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&woke));
        let mut lent = Box::pin(src.read_owned(exactly(4)));
        assert!(
            lent.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        send(&peer, b"abcdefgh");
        until_or(
            || woke.0.load(Ordering::SeqCst),
            || {
                format!(
                    "the driver never filled the lent buffer: {}",
                    src.read_report()
                )
            },
        )
        .await;
        let mut first = [0u8; 4];
        let mut early = Box::pin(read(&src, &mut first));
        pends(&mut early).await;
        let before = src.read_report();
        drop(lent);
        let after = src.read_report();
        // The state on both sides of the drop, should the read never end.
        match crate::time::timeout(Duration::from_secs(5), early).await {
            Ok(n) => assert_eq!(n.unwrap(), 4),
            Err(_) => panic!(
                "the read never ended; before the drop: {before}; after: {after}; now: {}",
                src.read_report()
            ),
        }
        let mut rest = [0u8; 4];
        match crate::time::timeout(Duration::from_secs(5), read_exact(&src, &mut rest)).await {
            Ok(done) => done.unwrap(),
            Err(_) => panic!("the rest never came: {}", src.read_report()),
        }
        (first, rest)
    });
    assert_eq!((&first, &rest), (b"abcd", b"efgh"));
    drop(a);
}

#[test]
fn a_lent_read_dropped_before_any_byte_loses_nothing() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        let mut lent = Box::pin(b.src.read_owned(Vec::with_capacity(16)));
        pends(&mut lent).await;
        drop(lent);
        a.io().write_all(b"kept").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(within(read(&b.src, &mut buf)).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"kept");
    });
}

/// One buffer is lent at a time: a second lent read waits its turn, and
/// gets the bytes after the first's.
#[test]
fn two_lent_reads_take_their_turns_in_order() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src, peer) = (Arc::clone(&b.src), a.io().try_clone().unwrap());
    let (first, second) = on_worker(&rt, async move {
        let mut one = Box::pin(src.read_owned(exactly(4)));
        let mut two = Box::pin(src.read_owned(exactly(4)));
        pends(&mut one).await;
        pends(&mut two).await;
        send(&peer, b"abcdefgh");
        let (n1, b1) = one.await;
        let (n2, b2) = two.await;
        assert_eq!((n1.unwrap(), n2.unwrap()), (4, 4));
        (b1, b2)
    });
    assert_eq!((&first[..], &second[..]), (&b"abcd"[..], &b"efgh"[..]));
    drop(a);
}

/// A plain read that waits behind a lent buffer gets the bytes after it.
#[test]
fn a_plain_read_waits_behind_a_lent_buffer() {
    let rt = runtime(None);
    let (a, b) = pair();
    let (src, peer) = (Arc::clone(&b.src), a.io().try_clone().unwrap());
    let (lent, plain) = on_worker(&rt, async move {
        let mut lent = Box::pin(src.read_owned(exactly(4)));
        pends(&mut lent).await;
        let reader = Arc::clone(&src);
        let plain = crate::runtime::spawn(async move {
            let mut buf = [0u8; 4];
            read_exact(&reader, &mut buf).await.unwrap();
            buf
        });
        send(&peer, b"abcdefgh");
        let (n, buf) = lent.await;
        assert_eq!(n.unwrap(), 4);
        (buf, plain.await)
    });
    assert_eq!((&lent[..], &plain), (&b"abcd"[..], b"efgh"));
    drop(a);
}

#[test]
fn a_lent_write_goes_out_whole_from_the_buffer() {
    let rt = runtime(None);
    let (a, b) = pair();
    let data = pattern(1000);
    let (src, expected) = (Arc::clone(&a.src), data.clone());
    let (n, back, charged) = on_worker(&rt, async move {
        let (n, back) = src.write_owned(data).await;
        (n.unwrap(), back, pool_charged())
    });
    assert_eq!(n, 1000);
    assert_eq!(back, expected, "the buffer came back as it was");
    assert_eq!(charged, 0, "the bytes went through a pool buffer");
    let mut got = vec![0u8; 1000];
    b.io().read_exact(&mut got).unwrap();
    assert_eq!(got, expected);
}

/// Larger than the socket takes at once: the driver writes the rest on the
/// socket's edges, and a lent read on the other end takes it all, so no
/// pool buffer is used on either side.
#[test]
fn a_lent_write_past_the_socket_buffer_arrives_whole_and_in_order() {
    let rt = runtime(None);
    let (a, b) = pair();
    let data = pattern(4 << 20);
    let expected = data.clone();
    let (writer, reader) = (Arc::clone(&a.src), Arc::clone(&b.src));
    let (n, got, charged) = on_worker(&rt, async move {
        let len = data.len();
        let read = crate::runtime::spawn(async move {
            let mut got = Vec::with_capacity(len);
            while got.len() < len {
                let (n, back) = reader.read_owned(got).await;
                assert!(n.unwrap() > 0, "the stream ended early");
                got = back;
            }
            got
        });
        let (n, _) = writer.write_owned(data).await;
        let got = read.await;
        (n.unwrap(), got, pool_charged())
    });
    assert_eq!(n, expected.len());
    assert!(got == expected, "the bytes arrived out of order or changed");
    assert_eq!(charged, 0, "the bytes went through a pool buffer");
}

/// Bytes written behind go out before a lent buffer, and a write after it
/// waits until the lent buffer is out.
#[test]
fn a_lent_write_keeps_its_place_among_copied_writes() {
    let rt = runtime(None);
    let (a, b) = pair();
    let reader = Arc::clone(&b.src);
    let filler = fill(a.io());
    let (src, behind, lent, after) = (
        Arc::clone(&a.src),
        pattern(1000),
        pattern(300_000),
        b"after".to_vec(),
    );
    let mut expected = filler.clone();
    expected.extend_from_slice(&behind);
    expected.extend_from_slice(&lent);
    expected.extend_from_slice(&after);
    let got = on_worker(&rt, async move {
        within(write_all(&src, &behind)).await.unwrap();
        assert!(src.has_write_behind(), "the copied bytes went out at once");
        let mut owned = Box::pin(src.write_owned(lent));
        pends(&mut owned).await;
        let mut later = Box::pin(write_all(&src, &after));
        pends(&mut later).await;
        let total = expected.len();
        let read = crate::runtime::spawn(async move {
            let mut got = vec![0u8; total];
            read_exact(&reader, &mut got).await.unwrap();
            got
        });
        let (n, _) = owned.await;
        assert_eq!(n.unwrap(), 300_000);
        later.await.unwrap();
        flush(&src).await.unwrap();
        (read.await, expected)
    });
    assert!(got.0 == got.1, "the writes went out of order");
}

/// The socket takes nothing, and the lent write's future is dropped: none
/// of the buffer ever goes out. A Unix socket's buffers are fixed, so a
/// full one takes nothing; Windows' TCP grows its send buffer and takes a
/// send even then, so there the withdrawal is checked on pipes (see
/// `fs::tests::windows`).
#[cfg(unix)]
#[test]
fn a_dropped_lent_write_that_sent_nothing_is_withdrawn() {
    let rt = runtime(None);
    let (a, b) = pair();
    let filler = fill(a.io());
    rt.block_on(async {
        let mut owned = Box::pin(a.src.write_owned(vec![9u8; 1000]));
        pends(&mut owned).await;
        drop(owned);
        // Withdrawn at once by a readiness driver; on a ring once the
        // kernel says the cancelled send took nothing.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut backoff = Duration::from_millis(1);
        while a.src.has_unsent() {
            assert!(
                Instant::now() < deadline,
                "the withdrawn write is still pending"
            );
            crate::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(50));
        }
    });
    a.io().shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = Vec::new();
    b.io().set_nonblocking(false).unwrap();
    b.io().read_to_end(&mut got).unwrap();
    assert_eq!(
        got.len(),
        filler.len(),
        "bytes of the withdrawn write arrived"
    );
}

/// Some of the buffer went out when the future was dropped: the driver
/// writes the rest, a flush waits for it, and later writes follow it.
#[test]
fn a_dropped_lent_write_that_started_is_finished_by_the_driver() {
    let rt = runtime(None);
    let (a, b) = pair();
    let data = pattern(4 << 20);
    let mut expected = data.clone();
    expected.extend_from_slice(b"tail");
    let (writer, reader) = (Arc::clone(&a.src), Arc::clone(&b.src));
    let got = on_worker(&rt, async move {
        let mut owned = Box::pin(writer.write_owned(data));
        pends(&mut owned).await;
        drop(owned);
        assert!(writer.has_unsent(), "the started write was withdrawn");
        let total = expected.len();
        let read = crate::runtime::spawn(async move {
            let mut got = vec![0u8; total];
            read_exact(&reader, &mut got).await.unwrap();
            got
        });
        within(flush(&writer)).await.unwrap();
        assert!(!writer.has_unsent());
        within(write_all(&writer, b"tail")).await.unwrap();
        (read.await, expected)
    });
    assert!(got.0 == got.1, "the write was torn or out of order");
}

#[test]
fn a_lent_write_to_a_gone_peer_fails_and_gives_the_buffer_back() {
    let rt = runtime(None);
    let (a, b) = pair();
    drop(b);
    rt.block_on(async {
        // A Unix socket fails the first write; TCP takes it, and fails the
        // ones after the peer's reset.
        let (err, back) = within(async {
            loop {
                let (n, back) = a.src.write_owned(vec![1u8; 100]).await;
                match n {
                    Err(err) => break (err, back),
                    Ok(_) => crate::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await;
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) || cfg!(windows) && err.kind() == io::ErrorKind::ConnectionAborted,
            "{err:?}"
        );
        assert_eq!(back, vec![1u8; 100]);
    });
}

/// A handle dropped while the driver still writes a lent buffer whose
/// future went away: the rest goes out through its copy of the descriptor.
#[test]
fn a_dropped_handle_delivers_the_rest_of_a_started_lent_write() {
    let rt = runtime(None);
    let (a, b) = stream_pair();
    let data = pattern(4 << 20);
    let expected = data.clone();
    let got = on_worker(&rt, async move {
        let a = Handle::new(a).unwrap();
        let b = Handle::new(b).unwrap();
        let mut owned = Box::pin(a.write_owned(data));
        pends(&mut owned).await;
        drop(owned);
        drop(a);
        let reader = Arc::clone(b.source());
        let got = read_to_end(&reader).await;
        drop(b);
        got
    });
    assert!(got == expected, "the bytes after the drop were lost");
}

#[test]
fn tcp_streams_read_and_write_lent_buffers() {
    let rt = runtime(None);
    let got = on_worker(&rt, async {
        let listener = Handle::<std::net::TcpListener>::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = crate::runtime::spawn(async move {
            let stream = Handle::<std::net::TcpStream>::connect(addr).await.unwrap();
            let (n, mut back) = stream.write_owned(b"ping".to_vec()).await;
            assert_eq!(n.unwrap(), 4);
            back.reserve(16);
            let (n, reply) = stream.read_owned(back).await;
            assert_eq!(n.unwrap(), 4);
            reply
        });
        let (server, _) = listener.accept().await.unwrap();
        let (n, buf) = server.read_owned(Vec::with_capacity(16)).await;
        assert_eq!((n.unwrap(), &buf[..]), (4, &b"ping"[..]));
        let (n, _) = server.write_owned(b"pong".to_vec()).await;
        assert_eq!(n.unwrap(), 4);
        client.await
    });
    assert_eq!(
        got, b"pingpong",
        "the reply goes after what the buffer held"
    );
}
