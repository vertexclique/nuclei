//! Syncing a directory: the call, the answers for paths that are not
//! directories, and the atomic replace of a file.

use super::*;
use crate::fs::{rename, sync_dir};

#[test]
fn a_directory_syncs() {
    let rt = runtime();
    let tmp = Temp::new("dir-sync");
    std::fs::write(tmp.file("entry"), b"a name in the directory").unwrap();
    rt.block_on(async {
        within(sync_dir(&tmp.0)).await.unwrap();
        within(sync_dir(tmp.0.to_str().unwrap())).await.unwrap();
        within(sync_dir(tmp.0.clone())).await.unwrap();
    });
}

#[test]
fn a_path_that_is_missing_is_not_found_as_open_says() {
    let rt = runtime();
    let tmp = Temp::new("dir-missing");
    let missing = tmp.file("nothing");
    let theirs = File::open(&missing).unwrap_err();
    rt.block_on(async {
        let err = within(sync_dir(&missing)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.raw_os_error(), theirs.raw_os_error());
    });
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_a_directory_gets_the_systems_answer() {
    let rt = runtime();
    let tmp = Temp::new("dir-file");
    let path = tmp.file("plain");
    std::fs::write(&path, b"not a directory").unwrap();
    let theirs = File::open(&path).and_then(|file| file.sync_all());
    rt.block_on(async {
        let ours = within(sync_dir(&path)).await;
        assert_eq!(ours.is_ok(), theirs.is_ok());
        if let (Err(ours), Err(theirs)) = (ours, theirs) {
            assert_eq!(ours.raw_os_error(), theirs.raw_os_error());
        }
    });
}

#[test]
fn a_file_is_replaced_atomically_by_the_recipe() {
    let rt = runtime();
    let tmp = Temp::new("dir-replace");
    let target = tmp.file("target");
    std::fs::write(&target, b"old contents").unwrap();
    rt.block_on(async {
        let temporary = tmp.file("target.tmp");
        let mut file = within(Handle::<File>::create(&temporary)).await.unwrap();
        within(write_all(&mut file, b"new contents")).await.unwrap();
        within(file.sync_all()).await.unwrap();
        drop(file);
        within(rename(&temporary, &target)).await.unwrap();
        within(sync_dir(&tmp.0)).await.unwrap();
    });
    assert_eq!(std::fs::read(&target).unwrap(), b"new contents");
    assert!(!tmp.file("target.tmp").exists());
}
