//! Unix pipes: an anonymous pair ([`pipe`]) and FIFOs opened by path
//! ([`OpenOptions`]), as a [`Sender`] (the write end) and a [`Receiver`]
//! (the read end). Each is a [`Handle`] of std's pipe end, read or written by
//! the driver as any byte stream, with the calls of its one direction.
//!
//! ```
//! use nuclei::io::{AsyncReadExt, AsyncWriteExt};
//! use nuclei::net::unix::pipe;
//!
//! # nuclei::block_on(async {
//! let (mut tx, mut rx) = pipe::pipe()?;
//! tx.write_all(b"through").await?;
//! tx.flush().await?;
//! let mut buf = [0u8; 7];
//! rx.read_exact(&mut buf).await?;
//! assert_eq!(&buf, b"through");
//! # std::io::Result::Ok(())
//! # }).unwrap();
//! ```

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

use crate::io::Handle;

/// A pipe's write end.
pub type Sender = Handle<std::io::PipeWriter>;
/// A pipe's read end.
pub type Receiver = Handle<std::io::PipeReader>;

/// A new anonymous pipe: what is written to the sender is read from the
/// receiver.
///
/// # Errors
///
/// When the process has no descriptors left.
pub fn pipe() -> io::Result<(Sender, Receiver)> {
    let (rx, tx) = std::io::pipe()?;
    Ok((Handle::new(tx)?, Handle::new(rx)?))
}

/// How a FIFO is opened by path.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read_write: bool,
    unchecked: bool,
}

impl OpenOptions {
    /// Options with each set to its default.
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }

    /// Opens the FIFO for reading and writing both (Linux): a receiver
    /// sees no end while it holds the write side itself, and a sender
    /// opens with no reader there yet. Default false.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn read_write(&mut self, value: bool) -> &mut Self {
        self.read_write = value;
        self
    }

    /// Opens the path without checking it is a FIFO. Default false.
    pub fn unchecked(&mut self, value: bool) -> &mut Self {
        self.unchecked = value;
        self
    }

    /// Opens the FIFO at `path` as a receiver.
    ///
    /// # Errors
    ///
    /// When the path cannot be opened, or (unless unchecked) is not a FIFO.
    pub fn open_receiver<P: AsRef<Path>>(&self, path: P) -> io::Result<Receiver> {
        let fd = self.open(path.as_ref(), false)?;
        Handle::new(std::io::PipeReader::from(fd))
    }

    /// Opens the FIFO at `path` as a sender.
    ///
    /// # Errors
    ///
    /// When the path cannot be opened (`ENXIO` when nothing reads it yet,
    /// unless opened for reading and writing), or (unless unchecked) is not
    /// a FIFO.
    pub fn open_sender<P: AsRef<Path>>(&self, path: P) -> io::Result<Sender> {
        let fd = self.open(path.as_ref(), true)?;
        Handle::new(std::io::PipeWriter::from(fd))
    }

    fn open(&self, path: &Path, write: bool) -> io::Result<OwnedFd> {
        let mut options = std::fs::OpenOptions::new();
        // Non-blocking from the open: a FIFO's open waits for its other end
        // otherwise.
        options.custom_flags(libc::O_NONBLOCK);
        if self.read_write {
            options.read(true).write(true);
        } else if write {
            options.write(true);
        } else {
            options.read(true);
        }
        let file: File = options.open(path)?;
        if !self.unchecked && !file.metadata()?.file_type().is_fifo() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path is not a FIFO",
            ));
        }
        Ok(file.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{AsyncReadExt, AsyncWriteExt};

    fn fifo(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("nuclei-fifo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a plain call with a valid path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        path
    }

    #[test]
    fn an_anonymous_pipe_carries_bytes_to_its_end() {
        crate::block_on(async {
            let (mut tx, mut rx) = pipe().unwrap();
            let mut buf = [0u8; 8];
            assert_eq!(
                rx.try_read(&mut buf).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            tx.write_all(b"bytes").await.unwrap();
            tx.flush().await.unwrap();
            rx.readable().await.unwrap();
            assert_eq!(rx.try_read(&mut buf).unwrap(), 5);
            drop(tx);
            assert_eq!(
                rx.read(&mut buf).await.unwrap(),
                0,
                "the end, once the sender went"
            );
        });
    }

    #[test]
    fn a_fifo_opens_by_path_and_is_checked() {
        let path = fifo("checked");
        crate::block_on(async {
            let mut rx = OpenOptions::new().open_receiver(&path).unwrap();
            let mut tx = OpenOptions::new().open_sender(&path).unwrap();
            tx.write_all(b"fifo").await.unwrap();
            tx.flush().await.unwrap();
            let mut buf = [0u8; 4];
            rx.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"fifo");
        });
        let _ = std::fs::remove_file(&path);
        let plain = std::env::temp_dir().join(format!("nuclei-not-fifo-{}", std::process::id()));
        std::fs::write(&plain, b"x").unwrap();
        let err = OpenOptions::new().open_receiver(&plain).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            OpenOptions::new()
                .unchecked(true)
                .open_receiver(&plain)
                .is_ok()
        );
        let _ = std::fs::remove_file(&plain);
    }

    #[test]
    fn a_sender_with_no_reader_is_refused_unless_it_reads_too() {
        let path = fifo("lonely");
        let err = OpenOptions::new().open_sender(&path).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENXIO));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert!(
            OpenOptions::new()
                .read_write(true)
                .open_sender(&path)
                .is_ok()
        );
        let _ = std::fs::remove_file(&path);
    }
}
