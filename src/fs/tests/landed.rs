//! Files opened to sync each write (`O_DSYNC`, `O_SYNC`): each write at the
//! cursor is Ready only once it landed, with its own result, and a write
//! whose future went away still lands, its answer going to a poll of the
//! same write (`proofs/tla/lent/LentFile.tla`, scenario "dsync").

use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use super::owned::{both, hold_the_pool, one_blocking_thread, pends};
use super::*;

fn synced(path: &PathBuf, flag: libc::c_int) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(flag)
        .open(path)
        .unwrap()
}

/// One `poll_write` of `buf`: how many bytes went, or the write's error.
async fn write_once<W: AsyncWrite + Unpin>(w: &mut W, buf: &[u8]) -> io::Result<usize> {
    poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, buf)).await
}

/// `len` bytes of a pattern of their own, told apart by `seed`.
fn seeded(len: usize, seed: u8) -> Vec<u8> {
    pattern(len).into_iter().map(|b| b ^ seed).collect()
}

#[test]
fn a_dsync_write_has_landed_when_it_answers() {
    let rt = runtime();
    let tmp = Temp::new("dsync-landed");
    let path = tmp.file("data");
    let data = pattern(200_000);
    rt.block_on(async {
        let mut file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        assert!(file.file().unwrap().lands_first());
        within(write_all(&mut file, &data)).await.unwrap();
        assert!(
            !file.file().unwrap().has_unsent(),
            "a write answered before it landed"
        );
        assert!(std::fs::read(&path).unwrap() == data);
    });
}

/// A write whose future went away while its call was out still lands; the
/// next write, of other bytes in the same memory, waits for it and then
/// makes its own call.
#[test]
fn a_write_dropped_with_its_call_out_still_lands_and_the_next_makes_its_own() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("dsync-dropped");
    let path = tmp.file("data");
    let (c1, c2) = (seeded(1000, 0x55), seeded(1000, 0xaa));
    rt.block_on(async {
        let mut file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let mut buf = c1.clone();
        let mut first = Box::pin(write_once(&mut file, &buf));
        pends(&mut first).await;
        drop(first);
        buf.copy_from_slice(&c2);
        let mut second = Box::pin(write_once(&mut file, &buf));
        pends(&mut second).await;
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(second).await.unwrap(), 1000);
        within(flush(&mut file)).await.unwrap();
    });
    assert!(std::fs::read(&path).unwrap() == [c1, c2].concat());
}

/// The same bytes at the same place, polled again after the future that
/// made the write went away, take that write's answer and make no call.
#[test]
fn a_write_polled_again_after_its_future_went_takes_the_same_write_s_answer() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("dsync-again");
    let path = tmp.file("data");
    let c1 = seeded(1000, 0x55);
    rt.block_on(async {
        let mut file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let buf = c1.clone();
        let mut first = Box::pin(write_once(&mut file, &buf));
        pends(&mut first).await;
        drop(first);
        let mut second = Box::pin(write_once(&mut file, &buf));
        pends(&mut second).await;
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(second).await.unwrap(), 1000);
        within(flush(&mut file)).await.unwrap();
    });
    assert!(std::fs::read(&path).unwrap() == c1);
}

/// A landed write whose answer waits is answered from it, not with a
/// failure the state took meanwhile from elsewhere; a new write is refused.
#[test]
fn a_write_s_own_answer_comes_before_another_failure() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("dsync-own-first");
    let path = tmp.file("data");
    let c1 = seeded(1000, 0x55);
    rt.block_on(async {
        let mut file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        let io = file.file().unwrap().clone();
        io.keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let buf = c1.clone();
        let mut first = Box::pin(write_once(&mut file, &buf));
        pends(&mut first).await;
        io.fail_for_test(io::Error::from(io::ErrorKind::BrokenPipe));
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(first).await.unwrap(), c1.len());
        assert!(std::fs::read(&path).unwrap() == c1);
        let err = within(write_once(&mut file, &seeded(1000, 0xaa)))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn two_tasks_writing_at_once_each_get_their_own_answer() {
    let rt = runtime();
    let tmp = Temp::new("dsync-two");
    let path = tmp.file("data");
    let (a, b) = (seeded(50_000, 0x11), seeded(30_000, 0x77));
    rt.block_on(async {
        let file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        let (mut one, mut two) = (&file, &file);
        let (x, y) = both(
            Box::pin(write_once(&mut one, &a)),
            Box::pin(write_once(&mut two, &b)),
        )
        .await;
        assert_eq!((x.unwrap(), y.unwrap()), (a.len(), b.len()));
    });
    let got = std::fs::read(&path).unwrap();
    assert!(
        got == [a.clone(), b.clone()].concat() || got == [b, a].concat(),
        "the file holds {} bytes, not both writes once each",
        got.len()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
#[test]
fn the_sync_flags_are_read_from_the_descriptor() {
    let rt = runtime();
    let tmp = Temp::new("dsync-flags");
    rt.block_on(async {
        let lands = |path: PathBuf, flag: libc::c_int| {
            let file = match flag {
                0 => read_write(&path),
                flag => synced(&path, flag),
            };
            let file = Handle::new(file).unwrap();
            let io = Arc::clone(file.file().unwrap());
            io.lands_first()
        };
        assert!(!lands(tmp.file("plain"), 0));
        assert!(lands(tmp.file("dsync"), libc::O_DSYNC));
        assert!(lands(tmp.file("sync"), libc::O_SYNC));
    });
}

/// A write that failed, its end taken into the answers by a poll of another
/// write, and left there as the handle goes, is said and counted as a write
/// that failed with no caller.
#[test]
fn a_failed_answer_left_when_the_handle_goes_is_counted() {
    if !crate::alone::alone(
        "fs::tests::landed::a_failed_answer_left_when_the_handle_goes_is_counted",
    ) {
        return;
    }
    let rt = one_blocking_thread();
    let tmp = Temp::new("dsync-left-failed");
    let path = tmp.file("data");
    let (c1, c2) = (seeded(1000, 0x55), seeded(1000, 0xaa));
    let before = crate::stats().write_behind_lost;
    rt.block_on(async {
        let mut file = Handle::new(synced(&path, libc::O_DSYNC)).unwrap();
        let io = file.file().unwrap().clone();
        io.keep_on_pool();
        io.fail_next_writes(1);
        let (hold, busy) = hold_the_pool();
        let mut buf = c1.clone();
        let mut first = Box::pin(write_once(&mut file, &buf));
        pends(&mut first).await;
        drop(first);
        buf.copy_from_slice(&c2);
        let mut second = Box::pin(write_once(&mut file, &buf));
        pends(&mut second).await;
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(second).await.unwrap(), 1000);
        drop(io);
        drop(file);
    });
    assert!(std::fs::read(&path).unwrap() == c2);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut backoff = Duration::from_millis(1);
    while crate::stats().write_behind_lost <= before {
        assert!(
            Instant::now() < deadline,
            "a failed answer left as the handle went was not counted"
        );
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}
