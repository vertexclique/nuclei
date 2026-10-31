//! Advisory file locks: which handles exclude which, the wait on the pool
//! and not the worker, the unlock after the writes, and a lock future that is
//! dropped at every point never leaves the file locked.

use std::fs::TryLockError;

use super::owned::{hold_the_pool, one_blocking_thread, pends};
use super::*;

fn open_rw(path: &PathBuf) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn handle(path: &PathBuf) -> Handle<File> {
    Handle::new(open_rw(path)).unwrap()
}

fn path_of(tag: &str) -> (Temp, PathBuf) {
    let tmp = Temp::new(tag);
    let path = tmp.file("data");
    std::fs::write(&path, pattern(1000)).unwrap();
    (tmp, path)
}

fn would_block(result: Result<(), TryLockError>) -> bool {
    match result {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(err)) => panic!("the lock failed: {err:?}"),
    }
}

/// Whether another handle of the file can take the lock now.
async fn free(path: &PathBuf) -> bool {
    let probe = handle(path);
    let taken = !would_block(probe.try_lock_file(false).await);
    if taken {
        probe.unlock_file().await.unwrap();
    }
    taken
}

/// Waits, to a deadline and with a bounded pause, until the lock is `free`
/// or held.
async fn becomes(path: &PathBuf, free_now: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while free(path).await != free_now {
        assert!(Instant::now() < deadline, "the lock never changed");
        crate::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn an_exclusive_lock_excludes_every_other_handle_until_it_is_released() {
    let rt = runtime();
    let (_tmp, path) = path_of("lock-exclusive");
    rt.block_on(async {
        let (a, b) = (handle(&path), handle(&path));
        within(a.lock_file(false)).await.unwrap();
        assert!(would_block(b.try_lock_file(false).await));
        assert!(would_block(b.try_lock_file(true).await));
        within(a.unlock_file()).await.unwrap();
        b.try_lock_file(false).await.unwrap();
        within(b.unlock_file()).await.unwrap();
    });
}

#[test]
fn shared_locks_share_and_exclude_an_exclusive_one() {
    let rt = runtime();
    let (_tmp, path) = path_of("lock-shared");
    rt.block_on(async {
        let (a, b, c) = (handle(&path), handle(&path), handle(&path));
        within(a.lock_file(true)).await.unwrap();
        b.try_lock_file(true).await.unwrap();
        assert!(would_block(c.try_lock_file(false).await));
        within(b.unlock_file()).await.unwrap();
        assert!(would_block(c.try_lock_file(false).await));
        within(a.unlock_file()).await.unwrap();
        c.try_lock_file(false).await.unwrap();
    });
}

#[test]
fn the_locks_answer_as_stds_do() {
    let rt = runtime();
    let (_tmp, path) = path_of("lock-std");
    let (theirs, other) = (open_rw(&path), open_rw(&path));
    theirs.lock().unwrap();
    let std_answer = other.try_lock();
    rt.block_on(async {
        let ours = handle(&path);
        assert!(matches!(std_answer, Err(TryLockError::WouldBlock)));
        assert!(would_block(ours.try_lock_file(false).await));
        theirs.unlock().unwrap();
        ours.try_lock_file(false).await.unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
        assert!(would_block(handle(&path).try_lock_file(true).await));
        within(ours.unlock_file()).await.unwrap();
        assert!(ours.try_lock().is_ok(), "std's own, through Deref");
        ours.unlock().unwrap();
    });
}

#[test]
fn a_waiting_lock_holds_a_pool_thread_and_not_the_worker() {
    let rt = one_blocking_thread();
    let (_tmp, path) = path_of("lock-waits");
    rt.block_on(async {
        let (a, b) = (handle(&path), handle(&path));
        within(a.lock_file(false)).await.unwrap();
        let mut waiting = Box::pin(b.lock_file(false));
        pends(&mut waiting).await;
        assert_eq!(within(spawn(async { 7 })).await, 7);
        within(a.unlock_file()).await.unwrap();
        within(waiting).await.unwrap();
        assert!(!free(&path).await);
        within(b.unlock_file()).await.unwrap();
        assert!(free(&path).await);
    });
}

#[test]
fn a_lock_dropped_while_its_job_waits_never_leaves_the_file_locked() {
    let rt = runtime();
    let (_tmp, path) = path_of("lock-dropped-waiting");
    rt.block_on(async {
        let (a, b) = (handle(&path), handle(&path));
        within(a.lock_file(false)).await.unwrap();
        let mut waiting = Box::pin(b.lock_file(false));
        pends(&mut waiting).await;
        drop(waiting);
        within(a.unlock_file()).await.unwrap();
        within(becomes(&path, true)).await;
    });
}

#[test]
fn a_lock_dropped_after_its_job_got_the_lock_gives_it_back() {
    let rt = one_blocking_thread();
    let (_tmp, path) = path_of("lock-dropped-held");
    rt.block_on(async {
        let b = handle(&path);
        let (hold, busy) = hold_the_pool();
        let mut taken = Box::pin(b.lock_file(false));
        pends(&mut taken).await;
        hold.send(()).unwrap();
        within(busy).await;
        within(becomes(&path, false)).await;
        drop(taken);
        assert!(free(&path).await, "the lock stayed held");
    });
}

#[test]
fn a_lock_dropped_before_its_job_started_never_locks() {
    let rt = one_blocking_thread();
    let (_tmp, path) = path_of("lock-dropped-queued");
    rt.block_on(async {
        let b = handle(&path);
        let (hold, busy) = hold_the_pool();
        let mut queued = Box::pin(b.lock_file(false));
        pends(&mut queued).await;
        drop(queued);
        hold.send(()).unwrap();
        within(busy).await;
        assert!(free(&path).await);
        within(becomes(&path, true)).await;
    });
}

#[test]
fn an_unlock_waits_for_the_writes_accepted_before_it() {
    let rt = runtime();
    let tmp = Temp::new("lock-unlock-writes");
    let path = tmp.file("data");
    let data = pattern(300_000);
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(file.lock_file(false)).await.unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        within(file.unlock_file()).await.unwrap();
        assert!(
            !file.file().unwrap().has_unsent(),
            "the unlock left writes behind"
        );
        assert!(std::fs::read(&path).unwrap() == data);
        assert!(free(&path).await);
    });
}

#[test]
fn an_unlock_releases_the_lock_when_a_write_failed_and_reports_it() {
    let rt = runtime();
    let (_tmp, path) = path_of("lock-unlock-failed");
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        within(file.lock_file(false)).await.unwrap();
        let written = within(write_all(&mut file, b"nope")).await;
        let unlocked = within(file.unlock_file()).await;
        assert!(
            written.is_err() || unlocked.is_err(),
            "the failed write was not reported"
        );
        assert!(free(&path).await, "the lock was kept");
    });
}
