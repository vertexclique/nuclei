//! File handles on a runtime: reads, writes, seeks and buffered reads at
//! the handle's cursor, what std sees of it, and the file handed back.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

use crate::io::Handle;
use crate::runtime::{Builder, Runtime, spawn};

fn runtime() -> Runtime {
    Builder::new()
        .worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap()
}

async fn within<F: Future>(fut: F) -> F::Output {
    crate::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("the operation never completed")
}

async fn read_some<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| Pin::new(&mut *r).poll_read(cx, buf)).await
}

async fn read_to_end<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 10_000];
    loop {
        match read_some(r, &mut buf).await? {
            0 => return Ok(out),
            n => out.extend_from_slice(&buf[..n]),
        }
    }
}

async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, buf)).await?;
        buf = &buf[n..];
    }
    Ok(())
}

async fn flush<W: AsyncWrite + Unpin>(w: &mut W) -> io::Result<()> {
    poll_fn(|cx| Pin::new(&mut *w).poll_flush(cx)).await
}

async fn seek<S: AsyncSeek + Unpin>(s: &mut S, pos: SeekFrom) -> io::Result<u64> {
    poll_fn(|cx| Pin::new(&mut *s).poll_seek(cx, pos)).await
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 997) as u8).collect()
}

/// A fresh file path, removed with its directory when the guard drops.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nuclei-fs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_write(path: &PathBuf) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap()
}

#[test]
fn a_file_reads_whole_through_the_pool() {
    let rt = runtime();
    let tmp = Temp::new("read");
    let path = tmp.file("data");
    let data = pattern(3 << 20 | 12345);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        let got = within(read_to_end(&mut file)).await.unwrap();
        assert!(got == data, "{} bytes read of {}", got.len(), data.len());
        // At the end, a read returns zero again.
        let mut buf = [0u8; 8];
        assert_eq!(within(read_some(&mut file, &mut buf)).await.unwrap(), 0);
    });
}

#[test]
fn bytes_written_are_read_back_after_a_seek() {
    let rt = runtime();
    let tmp = Temp::new("write");
    let path = tmp.file("data");
    let data = pattern(2 << 20);
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        // The seek waits for the writes, so the read sees them.
        assert_eq!(
            within(seek(&mut file, SeekFrom::Start(0))).await.unwrap(),
            0
        );
        let got = within(read_to_end(&mut file)).await.unwrap();
        assert!(got == data);
        within(flush(&mut file)).await.unwrap();
    });
    assert!(std::fs::read(&path).unwrap() == data, "std sees every byte");
}

#[test]
fn a_seek_from_the_end_and_from_the_cursor() {
    let rt = runtime();
    let tmp = Temp::new("seek");
    let path = tmp.file("data");
    std::fs::write(&path, b"0123456789").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        assert_eq!(within(seek(&mut file, SeekFrom::End(-3))).await.unwrap(), 7);
        let mut buf = [0u8; 8];
        let n = within(read_some(&mut file, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"789");
        assert_eq!(
            within(seek(&mut file, SeekFrom::Current(-5)))
                .await
                .unwrap(),
            5
        );
        let n = within(read_some(&mut file, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"56789");
        let err = within(seek(&mut file, SeekFrom::Current(-100)))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    });
}

#[test]
fn buffered_reads_hand_out_the_read_ahead() {
    let rt = runtime();
    let tmp = Temp::new("bufread");
    let path = tmp.file("lines");
    std::fs::write(&path, b"first\nsecond\nthird").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        let mut lines = Vec::new();
        let mut line = Vec::new();
        loop {
            let chunk = within(poll_fn(|cx| {
                Pin::new(&mut file).poll_fill_buf(cx).map_ok(<[u8]>::to_vec)
            }))
            .await
            .unwrap();
            if chunk.is_empty() {
                break;
            }
            let take = chunk
                .iter()
                .position(|&b| b == b'\n')
                .map_or(chunk.len(), |i| i + 1);
            line.extend_from_slice(&chunk[..take]);
            Pin::new(&mut file).consume(take);
            if line.ends_with(b"\n") {
                lines.push(core::mem::take(&mut line));
            }
        }
        lines.push(line);
        assert_eq!(lines, [&b"first\n"[..], b"second\n", b"third"]);
    });
}

#[test]
fn a_wrapped_file_goes_on_from_its_offset_and_goes_back_at_the_cursor() {
    let rt = runtime();
    let tmp = Temp::new("offset");
    let path = tmp.file("data");
    std::fs::write(&path, b"abcdefghij").unwrap();
    let mut std_file = File::open(&path).unwrap();
    let mut head = [0u8; 3];
    std_file.read_exact(&mut head).unwrap();
    let mut back = rt.block_on(async {
        let mut file = Handle::new(std_file).unwrap();
        let mut buf = [0u8; 4];
        let n = within(read_some(&mut file, &mut buf)).await.unwrap();
        assert_eq!(
            &buf[..n],
            b"defghij"[..n].as_ref(),
            "the handle goes on where std was"
        );
        let consumed = n as u64;
        (file.into_inner(), 3 + consumed)
    });
    assert_eq!(
        back.0.stream_position().unwrap(),
        back.1,
        "std goes on at the handle's cursor"
    );
    let mut rest = Vec::new();
    back.0.read_to_end(&mut rest).unwrap();
    assert_eq!(rest.len() as u64, 10 - back.1);
}

#[test]
fn a_file_dropped_with_writes_in_flight_still_gets_them() {
    let rt = runtime();
    let tmp = Temp::new("dropwrite");
    let path = tmp.file("data");
    let data = pattern(4 << 20);
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        drop(file);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&path).unwrap().len() < data.len() as u64 {
        assert!(Instant::now() < deadline, "the writes never landed");
        std::thread::yield_now();
    }
    assert!(std::fs::read(&path).unwrap() == data);
}

#[test]
fn two_tasks_reading_one_file_share_its_cursor() {
    let rt = runtime();
    let tmp = Temp::new("shared");
    let path = tmp.file("data");
    let data = pattern(1 << 20);
    std::fs::write(&path, &data).unwrap();
    let total = rt.block_on(async {
        let file = std::sync::Arc::new(within(Handle::<File>::open(&path)).await.unwrap());
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let file = std::sync::Arc::clone(&file);
                spawn(async move {
                    let mut r = &*file;
                    let mut got = 0;
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match read_some(&mut r, &mut buf).await.unwrap() {
                            0 => return got,
                            n => got += n,
                        }
                    }
                })
            })
            .collect();
        let mut total = 0;
        for r in readers {
            total += within(r).await;
        }
        total
    });
    assert_eq!(
        total,
        data.len(),
        "each byte was read once, by one task or the other"
    );
}

#[test]
fn a_failed_write_fails_the_next_writes_until_a_flush_reports_it() {
    let rt = runtime();
    let tmp = Temp::new("failwrite");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        // Opened for reading only: the job's pwrite fails.
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        let first = within(write_all(&mut file, b"nope")).await;
        let failed = match first {
            Err(err) => err,
            Ok(()) => within(flush(&mut file)).await.unwrap_err(),
        };
        assert!(failed.raw_os_error().is_some(), "{failed:?}");
    });
}

#[cfg(unix)]
#[test]
fn a_fifo_opened_as_a_file_is_watched_by_the_poller() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let rt = runtime();
    let tmp = Temp::new("fifo");
    let path = tmp.file("pipe");
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a plain syscall with our own path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let reader = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();
    // A FIFO with no writer reads as its end: the writer opens first.
    let mut w = OpenOptions::new().write(true).open(&path).unwrap();
    rt.block_on(async {
        let mut reader = Handle::new(reader).unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            w.write_all(b"through the fifo").unwrap();
        });
        let got = within(read_to_end(&mut reader)).await.unwrap();
        assert_eq!(got, b"through the fifo");
        writer.join().unwrap();
    });
}

mod allocate;
mod copy_range;
#[cfg(target_os = "linux")]
mod direct;
mod durable;
mod owned;
mod positional;
mod sync_dir;
mod sync_range;
#[cfg(windows)]
mod windows;
