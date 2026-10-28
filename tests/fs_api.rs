//! `nuclei::fs`: the calls by path, open options, a file's own calls, and
//! directories read a page at a time.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use nuclei::fs::{self, DirBuilder, File, OpenOptions};
use nuclei::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// A directory of this test's own, removed when it drops.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nuclei-fs-api-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn whole_files_are_written_read_copied_renamed_and_removed() {
    nuclei::block_on(async {
        let t = Temp::new();
        let a = t.path("a.txt");
        fs::write(&a, b"hello").await.unwrap();
        assert_eq!(fs::read(&a).await.unwrap(), b"hello");
        assert_eq!(fs::read_to_string(&a).await.unwrap(), "hello");
        assert_eq!(fs::metadata(&a).await.unwrap().len(), 5);
        assert!(fs::try_exists(&a).await.unwrap());

        let b = t.path("b.txt");
        assert_eq!(fs::copy(&a, &b).await.unwrap(), 5);
        let c = t.path("c.txt");
        fs::rename(&b, &c).await.unwrap();
        assert!(!fs::try_exists(&b).await.unwrap());
        assert_eq!(fs::read(&c).await.unwrap(), b"hello");

        let linked = t.path("hard.txt");
        fs::hard_link(&c, &linked).await.unwrap();
        assert_eq!(fs::read(&linked).await.unwrap(), b"hello");

        fs::remove_file(&c).await.unwrap();
        assert_eq!(fs::read(&c).await.unwrap_err().kind(), ErrorKind::NotFound);
        assert!(!fs::try_exists(&c).await.unwrap());

        let canonical = fs::canonicalize(&a).await.unwrap();
        assert!(canonical.is_absolute());
        assert!(canonical.ends_with("a.txt"));

        assert_eq!(
            fs::read_to_string(t.path("missing"))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    });
}

#[test]
fn directories_are_made_listed_a_page_at_a_time_and_removed() {
    nuclei::block_on(async {
        let t = Temp::new();
        let deep = t.path("x/y/z");
        fs::create_dir_all(&deep).await.unwrap();
        assert!(fs::metadata(&deep).await.unwrap().is_dir());
        fs::create_dir(t.path("x/y/w")).await.unwrap();
        DirBuilder::new()
            .recursive(true)
            .create(t.path("x/built/deeper"))
            .await
            .unwrap();
        assert!(
            fs::metadata(t.path("x/built/deeper"))
                .await
                .unwrap()
                .is_dir()
        );
        assert_eq!(
            fs::create_dir(t.path("nope/deeper"))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );

        // More entries than one page, files and a directory.
        let listing = t.path("listing");
        fs::create_dir(&listing).await.unwrap();
        let mut expected = BTreeSet::new();
        for i in 0..100 {
            let name = format!("f{i:03}");
            fs::write(listing.join(&name), [i as u8]).await.unwrap();
            expected.insert(name);
        }
        fs::create_dir(listing.join("sub")).await.unwrap();
        expected.insert("sub".to_string());

        let mut dir = fs::read_dir(&listing).await.unwrap();
        let mut seen = BTreeSet::new();
        let mut dirs = 0;
        while let Some(entry) = dir.next_entry().await.unwrap() {
            let name = entry.file_name().into_string().unwrap();
            assert_eq!(entry.path(), listing.join(&name));
            if entry.file_type().await.unwrap().is_dir() {
                dirs += 1;
            } else {
                assert_eq!(entry.metadata().await.unwrap().len(), 1);
            }
            assert!(seen.insert(name), "an entry came twice");
        }
        assert_eq!(seen, expected);
        assert_eq!(dirs, 1);
        assert!(dir.next_entry().await.unwrap().is_none(), "ended for good");

        fs::remove_dir(t.path("x/y/w")).await.unwrap();
        assert!(fs::remove_dir(t.path("x")).await.is_err(), "not empty");
        fs::remove_dir_all(t.path("x")).await.unwrap();
        assert!(!fs::try_exists(t.path("x")).await.unwrap());
    });
}

#[test]
fn read_dir_is_a_stream() {
    nuclei::block_on(async {
        let t = Temp::new();
        for i in 0..40 {
            fs::write(t.path(&format!("s{i}")), b"").await.unwrap();
        }
        let mut dir = fs::read_dir(&t.0).await.unwrap();
        let mut count = 0;
        while let Some(entry) = core::future::poll_fn(|cx| {
            futures_core::Stream::poll_next(std::pin::Pin::new(&mut dir), cx)
        })
        .await
        {
            entry.unwrap();
            count += 1;
        }
        assert_eq!(count, 40);
    });
}

#[test]
fn open_options_create_append_and_refuse() {
    nuclei::block_on(async {
        let t = Temp::new();
        let p = t.path("log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .await
            .unwrap();
        file.write_all(b"one\n").await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        let mut file = OpenOptions::new().append(true).open(&p).await.unwrap();
        file.write_all(b"two\n").await.unwrap();
        file.flush().await.unwrap();
        assert_eq!(fs::read_to_string(&p).await.unwrap(), "one\ntwo\n");

        let err = File::create_new(&p).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        let err = OpenOptions::new()
            .read(true)
            .open(t.path("absent"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    });
}

#[test]
fn a_files_calls_see_the_writes_in_flight() {
    nuclei::block_on(async {
        let t = Temp::new();
        let p = t.path("data");
        let mut file = File::create(&p).await.unwrap();
        file.write_all(&[7u8; 10_000]).await.unwrap();
        // No flush: the length and the sync see the write all the same.
        assert_eq!(file.metadata().await.unwrap().len(), 10_000);
        file.sync_all().await.unwrap();
        file.sync_data().await.unwrap();

        file.set_len(4).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 4);
        file.set_len(6).await.unwrap();
        assert_eq!(fs::read(&p).await.unwrap(), [7, 7, 7, 7, 0, 0]);

        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        file.set_modified(when).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().modified().unwrap(), when);
    });
}

#[test]
fn a_cloned_file_handle_reads_the_same_file() {
    nuclei::block_on(async {
        let t = Temp::new();
        let p = t.path("shared");
        fs::write(&p, b"0123456789").await.unwrap();
        let mut first = File::open(&p).await.unwrap();
        let mut second = first.try_clone().await.unwrap();
        let mut four = [0u8; 4];
        second.read_exact(&mut four).await.unwrap();
        assert_eq!(&four, b"0123");
        second.seek(std::io::SeekFrom::Start(8)).await.unwrap();
        let mut rest = Vec::new();
        second.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"89");
        let mut all = Vec::new();
        first.read_to_end(&mut all).await.unwrap();
        assert_eq!(all.len(), 10);
    });
}

#[cfg(unix)]
#[test]
fn links_permissions_and_modes_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    nuclei::block_on(async {
        let t = Temp::new();
        let target = t.path("target");
        fs::write(&target, b"x").await.unwrap();
        let link = t.path("link");
        fs::symlink(&target, &link).await.unwrap();
        assert_eq!(fs::read_link(&link).await.unwrap(), target);
        assert!(
            fs::symlink_metadata(&link)
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::metadata(&link).await.unwrap().is_file());

        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(&target).await.unwrap().permissions().mode() & 0o777,
            0o600
        );

        let made = t.path("made/inner");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&made)
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(&made).await.unwrap().permissions().mode() & 0o777,
            0o700
        );

        let created = t.path("mode");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o640)
            .open(&created)
            .await
            .unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o604))
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(&created).await.unwrap().permissions().mode() & 0o777,
            0o604
        );
        let mut dir = fs::read_dir(&t.0).await.unwrap();
        while let Some(entry) = dir.next_entry().await.unwrap() {
            assert!(entry.ino() > 0);
        }
    });
}

#[test]
fn the_calls_work_from_the_workers() {
    let rt = nuclei::runtime::Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap();
    rt.block_on(async {
        let t = Temp::new();
        let base = t.0.clone();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let base = base.clone();
                nuclei::spawn(async move {
                    let p = base.join(format!("w{i}"));
                    fs::write(&p, vec![i as u8; 1000]).await.unwrap();
                    fs::read(&p).await.unwrap()
                })
            })
            .collect();
        for (i, task) in tasks.into_iter().enumerate() {
            assert_eq!(task.await, vec![i as u8; 1000]);
        }
        let _ = Path::new(&base);
    });
}

/// A file opened to append writes at its end wherever its cursor is, and
/// after a write its cursor is the end of what it wrote, as std's is; a
/// write from the task's own buffer as well.
#[test]
fn an_appending_file_writes_at_its_end_and_its_cursor_follows() {
    nuclei::block_on(async {
        let t = Temp::new();
        let p = t.path("log");
        fs::write(&p, b"one\n").await.unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&p)
            .await
            .unwrap();
        file.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        file.write_all(b"two\n").await.unwrap();
        file.flush().await.unwrap();
        assert_eq!(file.stream_position().await.unwrap(), 8);
        let mut rest = Vec::new();
        assert_eq!(file.read_to_end(&mut rest).await.unwrap(), 0, "at the end");

        file.seek(std::io::SeekFrom::Start(2)).await.unwrap();
        let (written, _) = file.write_owned(b"three\n".to_vec()).await;
        assert_eq!(written.unwrap(), 6);
        assert_eq!(file.stream_position().await.unwrap(), 14);

        file.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        file.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"one\ntwo\nthree\n");

        // What std does with the same calls.
        use std::io::{Seek, Write};
        let mut std_file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&p)
            .unwrap();
        std_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        std_file.write_all(b"four\n").unwrap();
        assert_eq!(std_file.stream_position().unwrap(), 19);
        assert_eq!(fs::read(&p).await.unwrap(), b"one\ntwo\nthree\nfour\n");
    });
}
