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
    (near.unwrap(), far.unwrap().0)
}

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
            (s, Released::ready(back))
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

/// On a driver without zero-copy sends the write is copied, and its buffer is back at once.
#[test]
fn a_zero_copy_write_on_a_poller_is_copied_and_ready_at_once() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .backend(crate::proactor::POLLER)
        .build()
        .unwrap();
    // On a worker: the thread calling `block_on` runs a shard of its own, of any backend.
    rt.block_on(async {
        within(spawn(async {
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
