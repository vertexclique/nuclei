//! A file dropped with bytes in its write-behind: off a worker its drop
//! writes them before it returns, on a worker a linger does and the
//! process's exit waits for it, and bytes given up are said and counted
//! (`proofs/tla/going/Linger.tla`).

use super::*;

/// Where the process run alone writes, which the test that runs it reads.
const EXIT_PATH: &str = "NUCLEI_TEST_EXIT_PATH";

/// Waits, with a deadline and a backoff, until `stats` counts more given up
/// write-behinds than `before`.
fn counted_beyond(before: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut backoff = Duration::from_millis(1);
    loop {
        if crate::stats().write_behind_lost > before {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}

/// A file dropped in `block_on`'s own thread, which is no worker, has every
/// byte it accepted in the file as its drop returns, as std's would be.
#[test]
fn a_file_dropped_off_a_worker_is_whole_as_its_drop_returns() {
    let rt = runtime();
    let tmp = Temp::new("linger-off-worker");
    let path = tmp.file("data");
    let data = pattern(16 << 20 | 4093);
    rt.block_on(async {
        assert!(crate::runtime::worker_index().is_none());
        let mut file = within(Handle::<File>::create(&path)).await.unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        drop(file);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            len,
            data.len() as u64,
            "the drop returned before the bytes went"
        );
    });
    assert!(std::fs::read(&path).unwrap() == data);
}

/// A file dropped on a worker of the process default runtime, which is
/// never dropped, leaves a linger the process's exit waits for. Run in a
/// process of its own, whose one blocking thread is held as the file's last
/// write goes to it, the test returns and the process exits while that
/// write still waits: the file is whole after.
#[test]
fn the_exit_waits_for_a_file_dropped_on_a_worker() {
    let tmp = Temp::new("linger-exit");
    let path = tmp.file("data");
    let data = pattern(4 << 20 | 4093);
    let tail = data.len() - 4096;
    if crate::alone::alone_with(
        "fs::tests::linger::the_exit_waits_for_a_file_dropped_on_a_worker",
        &[(EXIT_PATH, path.as_os_str())],
    ) {
        let path = PathBuf::from(std::env::var_os(EXIT_PATH).unwrap());
        Builder::new()
            .max_blocking_threads(1)
            .build_global()
            .unwrap();
        crate::runtime::block_on(spawn(async move {
            assert!(crate::runtime::worker_index().is_some());
            let mut file = Handle::<File>::create(&path).await.unwrap();
            file.file().unwrap().keep_on_pool();
            write_all(&mut file, &data[..tail]).await.unwrap();
            flush(&mut file).await.unwrap();
            // Holds the one thread for longer than the exit takes, so the
            // last write is still out when the process exits.
            crate::runtime::spawn_blocking(|| std::thread::sleep(Duration::from_secs(1))).detach();
            write_all(&mut file, &data[tail..]).await.unwrap();
            assert!(file.file().unwrap().has_unsent());
            drop(file);
        }));
        return;
    }
    let written = std::fs::read(&path).unwrap();
    assert_eq!(
        written.len(),
        data.len(),
        "the exit did not wait for the linger"
    );
    assert!(written == data);
}

/// A file's write that fails with nobody left to answer, the handle dropped
/// on a worker or off one, is said at warn and counted.
#[test]
fn a_write_behind_given_up_is_counted() {
    let rt = runtime();
    let tmp = Temp::new("linger-counted");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    let before = crate::stats().write_behind_lost;
    let on_worker = path.clone();
    rt.block_on(async {
        within(spawn(async move {
            assert!(crate::runtime::worker_index().is_some());
            let mut file = Handle::<File>::open(&on_worker).await.unwrap();
            write_all(&mut file, b"lost").await.unwrap();
            drop(file);
        }))
        .await;
    });
    assert!(
        counted_beyond(before),
        "a linger's lost write was not counted"
    );
    let before = crate::stats().write_behind_lost;
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        within(write_all(&mut file, b"lost")).await.unwrap();
        drop(file);
    });
    assert!(
        crate::stats().write_behind_lost > before,
        "a drop off a worker did not count its lost write as it returned"
    );
}
