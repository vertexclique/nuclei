//! A lent read at the cursor whose bytes the page cache has is made at once,
//! with no job, where the filesystem takes `RWF_NOWAIT`; one whose bytes it
//! lacks goes to its job and reads them all the same (`fs::file::cached`).

use std::task::Poll;

use super::*;

/// Reads `file` to its end through lent reads of 64 KiB, each polled once
/// first: a read made at once must end at that poll.
async fn read_through(file: &Handle<File>) -> Vec<u8> {
    let mut got = Vec::new();
    loop {
        let mut read = Box::pin(file.read_owned(Vec::with_capacity(64 << 10)));
        let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
        let (n, buf) = match first {
            Poll::Ready(out) => out,
            Poll::Pending => within(read).await,
        };
        match n.unwrap() {
            0 => return got,
            _ => got.extend_from_slice(&buf),
        }
    }
}

/// Bytes just written are in the page cache: where the filesystem takes the
/// flag, every read of them ends at its first poll.
#[test]
fn a_lent_read_of_cached_bytes_ends_at_its_first_poll() {
    let rt = runtime();
    let tmp = Temp::new("cached");
    let path = tmp.file("data");
    let data = pattern(256 << 10);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(File::open(&path).unwrap()).unwrap();
        let mut got = Vec::new();
        loop {
            let mut read = Box::pin(file.read_owned(Vec::with_capacity(64 << 10)));
            let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
            let (n, buf) = match first {
                Poll::Ready(out) => out,
                Poll::Pending => {
                    assert!(
                        !file.file().unwrap().tries_cache(),
                        "a read of cached bytes waited for a job"
                    );
                    within(read).await
                }
            };
            match n.unwrap() {
                0 => break,
                _ => got.extend_from_slice(&buf),
            }
        }
        assert!(got == data, "the bytes read differ from the file's");
    });
}

/// Bytes the page cache dropped are read by the read's job, and the reads
/// after it find the cache again: the file reads whole and in order.
#[test]
fn a_lent_read_of_bytes_not_cached_reads_them_through_its_job() {
    use std::os::fd::AsRawFd;
    let rt = runtime();
    let tmp = Temp::new("uncached");
    let path = tmp.file("data");
    let data = pattern(1 << 20);
    {
        let mut file = File::create(&path).unwrap();
        std::io::Write::write_all(&mut file, &data).unwrap();
        file.sync_all().unwrap();
        // SAFETY: advice on the file's own descriptor; it reads nothing.
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "the advice was refused");
    }
    let got = rt.block_on(async {
        let file = Handle::new(File::open(&path).unwrap()).unwrap();
        read_through(&file).await
    });
    assert!(got == data, "the bytes read differ from the file's");
}
