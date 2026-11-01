//! A read into the task's own buffer waits on its own call's end, which
//! wakes the waker the read was polled with, and on the file's waiters only
//! while a write it must see is in flight (`poll_read_owned`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use super::owned::{hold_the_pool, one_blocking_thread, pends};
use super::*;

/// A waker that only records it was woken.
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Waits, the pause doubling up to 5 ms, until `flag` was woken.
async fn woken(flag: &Flag, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while !flag.0.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "{what}");
        crate::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// The read's end wakes the very waker the read was last polled with, on
/// the ring and on the blocking pool alike.
#[test]
fn a_lent_read_wakes_the_waker_it_was_polled_with() {
    for on_pool in [false, true] {
        let rt = one_blocking_thread();
        let tmp = Temp::new("lent-wake");
        let path = tmp.file("data");
        std::fs::write(&path, b"woken").unwrap();
        rt.block_on(async {
            let file = Handle::new(File::open(&path).unwrap()).unwrap();
            if on_pool {
                file.file().unwrap().keep_on_pool();
            }
            let (release, busy) = hold_the_pool();
            let mut read = Box::pin(file.read_owned(Vec::with_capacity(16)));
            let flag = Arc::new(Flag(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&flag));
            let first = read.as_mut().poll(&mut Context::from_waker(&waker));
            assert!(
                first.is_pending() || !on_pool,
                "the read on the held pool did not wait"
            );
            release.send(()).unwrap();
            within(busy).await;
            // Off the pool the page cache may have answered at once.
            let (n, got) = match first {
                Poll::Ready(out) => out,
                Poll::Pending => {
                    if !flag.0.load(Ordering::SeqCst) {
                        woken(&flag, "the read's end never woke its waker").await;
                    }
                    within(read).await
                }
            };
            assert_eq!(n.unwrap(), 5);
            assert_eq!(got, b"woken");
        });
    }
}

/// A lent read waits for the copied write in flight before it, is woken once
/// that write lands, and then reads on from where the write ended.
#[test]
fn a_lent_read_waiting_for_a_write_in_flight_reads_once_it_lands() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("lent-wake-write");
    let path = tmp.file("data");
    std::fs::write(&path, b"overwrittenthe rest").unwrap();
    rt.block_on(async {
        let open = OpenOptions::new().read(true).write(true).open(&path);
        let mut file = Handle::new(open.unwrap()).unwrap();
        // The test holds the pool: the write's call waits in its queue.
        file.file().unwrap().keep_on_pool();
        let (release, busy) = hold_the_pool();
        within(write_all(&mut file, b"landed here")).await.unwrap();
        let mut read = Box::pin(file.read_owned(Vec::with_capacity(32)));
        pends(&mut read).await;
        release.send(()).unwrap();
        within(busy).await;
        let (n, got) = within(read).await;
        assert_eq!(n.unwrap(), 8);
        assert_eq!(got, b"the rest");
    });
    assert_eq!(std::fs::read(&path).unwrap(), b"landed herethe rest");
}
