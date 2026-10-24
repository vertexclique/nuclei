//! Helpers the IO tests share: the `futures-io` calls as plain async
//! functions, and temporary files that remove themselves.

#![allow(dead_code)]

use std::future::poll_fn;
use std::io::{self, IoSlice, IoSliceMut, SeekFrom};
use std::path::PathBuf;
use std::pin::Pin;

use futures_io::{AsyncRead, AsyncSeek, AsyncWrite};

pub async fn read_to_end<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match poll_fn(|cx| Pin::new(&mut *r).poll_read(cx, &mut buf)).await? {
            0 => return Ok(out),
            n => out.extend_from_slice(&buf[..n]),
        }
    }
}

pub async fn read_vectored<R: AsyncRead + Unpin>(
    r: &mut R,
    bufs: &mut [IoSliceMut<'_>],
) -> io::Result<usize> {
    poll_fn(|cx| Pin::new(&mut *r).poll_read_vectored(cx, bufs)).await
}

pub async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, buf)).await?;
        buf = &buf[n..];
    }
    Ok(())
}

pub async fn write_vectored<W: AsyncWrite + Unpin>(
    w: &mut W,
    bufs: &[IoSlice<'_>],
) -> io::Result<usize> {
    poll_fn(|cx| Pin::new(&mut *w).poll_write_vectored(cx, bufs)).await
}

pub async fn flush<W: AsyncWrite + Unpin>(w: &mut W) -> io::Result<()> {
    poll_fn(|cx| Pin::new(&mut *w).poll_flush(cx)).await
}

pub async fn seek<S: AsyncSeek + Unpin>(s: &mut S, pos: SeekFrom) -> io::Result<u64> {
    poll_fn(|cx| Pin::new(&mut *s).poll_seek(cx, pos)).await
}

/// A file of the test's own under the system's temporary directory,
/// removed when this drops.
pub struct TempFile(pub PathBuf);

impl TempFile {
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "nuclei-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A file of `testdata/`.
pub fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}
