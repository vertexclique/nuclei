//! File reads and writes from the task's own buffer: the jobs read into it
//! and write from it at the cursor, in order with every other operation,
//! and a dropped future moves the cursor nowhere or lands its write whole.

#[cfg(unix)]
use std::os::fd::OwnedFd as OwnedFile;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle as OwnedFile;
use std::sync::Arc;

use super::*;

/// What this thread's pool took from the allocator.
fn pool_charged() -> usize {
    crate::buf::with_pool(|p| p.map_or(0, |p| p.charged()))
}

/// Polls `fut` once: it must wait.
pub(super) async fn pends<F: Future + Unpin>(fut: &mut F) {
    let polled = poll_fn(|cx| core::task::Poll::Ready(Pin::new(&mut *fut).poll(cx))).await;
    assert!(polled.is_pending(), "it did not wait");
}

/// A runtime whose blocking pool has one thread.
pub(super) fn one_blocking_thread() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .max_blocking_threads(1)
        .pin_workers(false)
        .build()
        .unwrap()
}

/// Holds the one thread of the pool of [`one_blocking_thread`] until the
/// sender sends: a file job submitted meanwhile waits in the pool's queue,
/// so it is out (and its future pending) for certain, not by timing. The
/// task ends once the thread is let go.
pub(super) fn hold_the_pool() -> (std::sync::mpsc::Sender<()>, crate::runtime::Task<()>) {
    let (hold, held) = std::sync::mpsc::channel::<()>();
    let busy = crate::runtime::spawn_blocking(move || {
        let _ = held.recv();
    });
    (hold, busy)
}

/// Both futures, polled together until both are done.
pub(super) async fn both<A: Future + Unpin, B: Future + Unpin>(
    mut a: A,
    mut b: B,
) -> (A::Output, B::Output) {
    let (mut x, mut y) = (None, None);
    poll_fn(|cx| {
        if x.is_none()
            && let core::task::Poll::Ready(out) = Pin::new(&mut a).poll(cx)
        {
            x = Some(out);
        }
        if y.is_none()
            && let core::task::Poll::Ready(out) = Pin::new(&mut b).poll(cx)
        {
            y = Some(out);
        }
        if x.is_some() && y.is_some() {
            core::task::Poll::Ready(())
        } else {
            core::task::Poll::Pending
        }
    })
    .await;
    (x.unwrap(), y.unwrap())
}

#[test]
fn a_lent_read_takes_the_file_at_the_cursor_with_no_pool_buffer() {
    let rt = runtime();
    let tmp = Temp::new("lent-read");
    let path = tmp.file("data");
    let data = pattern(3 << 20 | 777);
    std::fs::write(&path, &data).unwrap();
    let (got, charged) = rt.block_on(async {
        within(spawn(async move {
            let file = Handle::new(File::open(&path).unwrap()).unwrap();
            let mut got = Vec::with_capacity(data.len() + 1);
            got.push(b'>');
            loop {
                got.reserve(1 << 20);
                let (n, back) = file.read_owned(got).await;
                got = back;
                if n.unwrap() == 0 {
                    break;
                }
            }
            let charged = pool_charged();
            assert!(got[1..] == data[..], "the bytes differ from the file's");
            (got.len() - 1, charged)
        }))
        .await
    });
    assert_eq!(got, 3 << 20 | 777);
    assert_eq!(charged, 0, "the bytes went through a pool buffer");
}

#[test]
fn bytes_read_ahead_go_into_a_lent_buffer_first() {
    let rt = runtime();
    let tmp = Temp::new("lent-ahead");
    let path = tmp.file("data");
    std::fs::write(&path, b"hello world").unwrap();
    rt.block_on(async {
        let mut file = Handle::new(File::open(&path).unwrap()).unwrap();
        let mut six = [0u8; 6];
        assert_eq!(within(read_some(&mut file, &mut six)).await.unwrap(), 6);
        let mut buf = Vec::with_capacity(64);
        buf.push(b'>');
        let (n, buf) = within(file.read_owned(buf)).await;
        assert_eq!((n.unwrap(), &buf[..]), (5, &b">world"[..]));
        let (n, _) = within(file.read_owned(Vec::with_capacity(8))).await;
        assert_eq!(n.unwrap(), 0, "the end of the file");
        let full = vec![1u8; 2].into_boxed_slice().into_vec();
        let (n, back) = within(file.read_owned(full)).await;
        assert_eq!((n.unwrap(), &back[..]), (0, &[1u8, 1][..]));
    });
}

#[test]
fn a_lent_read_sees_the_writes_before_it() {
    let rt = runtime();
    let tmp = Temp::new("lent-sees");
    let path = tmp.file("data");
    let data = pattern(1 << 20);
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        let (n, _) = within(file.write_owned(b"tail".to_vec())).await;
        assert_eq!(n.unwrap(), 4);
        within(seek(&mut file, SeekFrom::Start(0))).await.unwrap();
        let (n, got) = within(file.read_owned(Vec::with_capacity(2 << 20))).await;
        assert_eq!(n.unwrap(), data.len() + 4);
        assert!(got[..data.len()] == data[..] && &got[data.len()..] == b"tail");
    });
}

#[test]
fn a_dropped_lent_read_moves_the_cursor_nowhere() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("lent-drop");
    let path = tmp.file("data");
    std::fs::write(&path, b"from the start").unwrap();
    rt.block_on(async {
        let mut file = Handle::new(File::open(&path).unwrap()).unwrap();
        // The test holds the pool: the file's calls go there.
        file.file().unwrap().keep_on_pool();
        let (release, busy) = hold_the_pool();
        let mut lent = Box::pin(file.read_owned(Vec::with_capacity(4)));
        pends(&mut lent).await;
        drop(lent);
        // The read's job runs only now, after its future went away.
        release.send(()).unwrap();
        within(busy).await;
        let got = within(read_to_end(&mut file)).await.unwrap();
        assert_eq!(got, b"from the start");
    });
}

/// Two lent reads at one cursor read the same bytes; the one whose result
/// comes second finds the cursor moved and reads on from there.
#[test]
fn two_lent_reads_at_once_take_consecutive_bytes() {
    let rt = runtime();
    let tmp = Temp::new("lent-two");
    let path = tmp.file("data");
    std::fs::write(&path, b"abcdefgh").unwrap();
    rt.block_on(async {
        let file = Handle::new(File::open(&path).unwrap()).unwrap();
        let exactly = || {
            let mut b = vec![0u8; 4].into_boxed_slice().into_vec();
            b.clear();
            b
        };
        let one = Box::pin(file.read_owned(exactly()));
        let two = Box::pin(file.read_owned(exactly()));
        let ((n1, b1), (n2, b2)) = within(both(one, two)).await;
        assert_eq!((n1.unwrap(), n2.unwrap()), (4, 4));
        let mut got = [b1, b2];
        got.sort();
        assert_eq!((&got[0][..], &got[1][..]), (&b"abcd"[..], &b"efgh"[..]));
    });
}

#[test]
fn lent_writes_land_in_order_among_copied_writes() {
    let rt = runtime();
    let tmp = Temp::new("lent-order");
    let path = tmp.file("data");
    let (a, b, c) = (pattern(300_000), vec![b'b'; 200_000], vec![b'c'; 100_000]);
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    expected.extend_from_slice(&c);
    expected.extend_from_slice(b"last");
    let got = rt.block_on(async {
        within(spawn(async move {
            let mut file = Handle::new(read_write(&path)).unwrap();
            within(write_all(&mut file, &a)).await.unwrap();
            let (n, back) = within(file.write_owned(b)).await;
            assert_eq!(n.unwrap(), 200_000);
            assert_eq!(back.len(), 200_000, "the buffer came back whole");
            // A copied write after a lent one that started goes after it.
            // The blocking pool may finish the lent write within its first
            // poll; it has started either way.
            let mut lent = Box::pin(file.write_owned(c));
            let first = poll_fn(|cx| core::task::Poll::Ready(Pin::new(&mut lent).poll(cx))).await;
            within(write_all(&mut &file, b"last")).await.unwrap();
            let (n, _) = match first {
                core::task::Poll::Ready(done) => {
                    drop(lent);
                    done
                }
                core::task::Poll::Pending => within(lent).await,
            };
            assert_eq!(n.unwrap(), 100_000);
            within(flush(&mut file)).await.unwrap();
            std::fs::read(&path).unwrap()
        }))
        .await
    });
    assert!(got == expected, "the writes landed out of order");
}

#[test]
fn a_lent_write_takes_no_pool_buffer() {
    let rt = runtime();
    let tmp = Temp::new("lent-nopool");
    let path = tmp.file("data");
    let data = pattern(1 << 20);
    let expected = data.clone();
    let charged = rt.block_on(async {
        let open = path.clone();
        within(spawn(async move {
            let mut file = Handle::new(read_write(&open)).unwrap();
            let (n, _) = within(file.write_owned(data)).await;
            assert_eq!(n.unwrap(), 1 << 20);
            within(flush(&mut file)).await.unwrap();
            pool_charged()
        }))
        .await
    });
    assert_eq!(charged, 0, "the bytes went through a pool buffer");
    assert!(std::fs::read(&path).unwrap() == expected);
}

/// A lent write waits for the write before it to land, even one that has
/// not started: with the one blocking thread busy, the copied write is
/// still queued when the lent one comes.
#[test]
fn a_lent_write_waits_for_a_write_still_queued() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("lent-queued");
    let path = tmp.file("data");
    let (a, b) = (pattern(100_000), vec![b'b'; 1000]);
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    rt.block_on(async {
        let (hold, busy) = hold_the_pool();
        let mut file = Handle::new(read_write(&path)).unwrap();
        // The test holds the pool: the file's calls go there.
        file.file().unwrap().keep_on_pool();
        within(write_all(&mut file, &a)).await.unwrap();
        let mut lent = Box::pin(file.write_owned(b));
        pends(&mut lent).await;
        hold.send(()).unwrap();
        within(busy).await;
        let (n, _) = within(lent).await;
        assert_eq!(n.unwrap(), 1000);
        within(flush(&mut file)).await.unwrap();
    });
    assert!(
        std::fs::read(&path).unwrap() == expected,
        "the queued write was lost"
    );
}

/// A lent write that started still lands when its future is dropped, and
/// a flush waits for it.
#[test]
fn a_dropped_lent_write_that_started_still_lands() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("lent-orphan");
    let path = tmp.file("data");
    let data = pattern(4 << 20);
    let expected = data.clone();
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        // The test holds the pool: the file's calls go there.
        file.file().unwrap().keep_on_pool();
        let (release, busy) = hold_the_pool();
        let mut lent = Box::pin(file.write_owned(data));
        pends(&mut lent).await;
        drop(lent);
        // The write's job runs only now, after its future went away.
        release.send(()).unwrap();
        within(busy).await;
        within(write_all(&mut file, b"after")).await.unwrap();
        within(flush(&mut file)).await.unwrap();
    });
    let got = std::fs::read(&path).unwrap();
    assert!(
        got.len() == expected.len() + 5 && got[..expected.len()] == expected[..],
        "{} bytes in the file",
        got.len()
    );
    assert_eq!(&got[expected.len()..], b"after");
}

/// A handle dropped while a lent write whose future went away is in flight:
/// the write lands all the same.
#[test]
fn a_dropped_handle_still_lands_a_started_lent_write() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("lent-handle");
    let path = tmp.file("data");
    let data = pattern(4 << 20);
    let expected = data.clone();
    let open = path.clone();
    rt.block_on(async move {
        let file = Handle::new(read_write(&open)).unwrap();
        // The test holds the pool: the file's calls go there.
        file.file().unwrap().keep_on_pool();
        let (release, busy) = hold_the_pool();
        let mut lent = Box::pin(file.write_owned(data));
        pends(&mut lent).await;
        drop(lent);
        drop(file);
        // The write's job runs only now, after its future and its handle went.
        release.send(()).unwrap();
        within(busy).await;
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while std::fs::read(&path).unwrap() != expected {
        assert!(Instant::now() < deadline, "the write never landed");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn a_lent_write_that_fails_gives_the_buffer_back_and_fails_until_a_flush() {
    let rt = runtime();
    let tmp = Temp::new("lent-fail");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        let mut file = Handle::new(File::open(&path).unwrap()).unwrap();
        let (n, back) = within(file.write_owned(b"nope".to_vec())).await;
        assert!(n.is_err(), "a write to a read-only file went through");
        assert_eq!(back, b"nope");
        let (n, _) = within(file.write_owned(b"again".to_vec())).await;
        assert!(n.is_err(), "the failure is kept for the next write");
        assert!(
            within(flush(&mut file)).await.is_err(),
            "the flush reports it"
        );
        let (n, _) = within(file.write_owned(Vec::new())).await;
        assert_eq!(n.unwrap(), 0);
    });
}

/// A file that is a pipe reads and writes lent buffers through the poller.
#[test]
fn a_pipe_opened_as_a_file_reads_and_writes_lent_buffers() {
    let rt = runtime();
    let (r, w) = std::io::pipe().unwrap();
    rt.block_on(async {
        let reader = Handle::new(File::from(OwnedFile::from(r))).unwrap();
        let writer = Arc::new(Handle::new(File::from(OwnedFile::from(w))).unwrap());
        let sender = Arc::clone(&writer);
        let sent = spawn(async move { sender.write_owned(b"through the pipe".to_vec()).await });
        let (n, got) = within(reader.read_owned(Vec::with_capacity(64))).await;
        assert_eq!((n.unwrap(), &got[..]), (16, &b"through the pipe"[..]));
        let (n, _) = within(sent).await;
        assert_eq!(n.unwrap(), 16);
    });
}
