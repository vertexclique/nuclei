//! [`read_dir`]: a directory's entries, streamed. The blocking pool reads
//! them a page of [`PAGE`] at a time, so a directory of any size costs a
//! page of memory, never its whole listing.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use futures_core::Stream;

use super::ops::blocking;
use crate::task::Join;

/// Entries read from the directory in one call of the blocking pool.
const PAGE: usize = 32;

/// One page and what reads the next.
type Page = (Option<std::fs::ReadDir>, VecDeque<io::Result<DirEntry>>);

/// The entries of the directory at `path`, through
/// [`ReadDir::next_entry`] or as a [`Stream`].
pub async fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref().to_path_buf();
    let dir = blocking(move || std::fs::read_dir(path)).await?;
    Ok(ReadDir {
        state: State::Idle(Some(dir), VecDeque::new()),
    })
}

/// A directory's entries, read a page at a time.
#[derive(Debug)]
pub struct ReadDir {
    state: State,
}

#[derive(Debug)]
enum State {
    /// The entries read and not handed out; `None` once the directory ended.
    Idle(Option<std::fs::ReadDir>, VecDeque<io::Result<DirEntry>>),
    /// The next page on the pool.
    Busy(Join<Page>),
}

impl ReadDir {
    /// The next entry, `None` at the end.
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        core::future::poll_fn(|cx| self.poll_next_entry(cx)).await
    }

    /// Polls for the next entry.
    pub fn poll_next_entry(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<DirEntry>>> {
        loop {
            match &mut self.state {
                State::Idle(dir, page) => {
                    if let Some(entry) = page.pop_front() {
                        return Poll::Ready(entry.map(Some));
                    }
                    let Some(mut dir) = dir.take() else {
                        return Poll::Ready(Ok(None));
                    };
                    self.state = State::Busy(
                        crate::runtime::spawn_blocking(move || {
                            let mut page = VecDeque::with_capacity(PAGE);
                            for _ in 0..PAGE {
                                match dir.next() {
                                    Some(entry) => page.push_back(entry.map(DirEntry::new)),
                                    None => return (None, page),
                                }
                            }
                            (Some(dir), page)
                        })
                        .join(),
                    );
                }
                State::Busy(job) => match ready!(Pin::new(job).poll(cx)) {
                    Ok((dir, page)) => self.state = State::Idle(dir, page),
                    Err(lost) => {
                        self.state = State::Idle(None, VecDeque::new());
                        return Poll::Ready(Err(lost.into()));
                    }
                },
            }
        }
    }
}

impl Stream for ReadDir {
    type Item = io::Result<DirEntry>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .poll_next_entry(cx)
            .map(|entry| entry.transpose())
    }
}

/// An entry of a directory.
#[derive(Debug)]
pub struct DirEntry {
    inner: std::sync::Arc<std::fs::DirEntry>,
    /// Read with the page: on most systems the directory read gives it at
    /// no cost.
    file_type: Result<FileType, (io::ErrorKind, String)>,
}

impl DirEntry {
    fn new(entry: std::fs::DirEntry) -> Self {
        let file_type = entry
            .file_type()
            .map_err(|err| (err.kind(), err.to_string()));
        Self {
            inner: std::sync::Arc::new(entry),
            file_type,
        }
    }

    /// The entry's full path: the directory's path joined with its name.
    pub fn path(&self) -> PathBuf {
        self.inner.path()
    }

    /// The entry's name in its directory.
    pub fn file_name(&self) -> OsString {
        self.inner.file_name()
    }

    /// The entry's inode number.
    #[cfg(unix)]
    pub fn ino(&self) -> u64 {
        std::os::unix::fs::DirEntryExt::ino(&*self.inner)
    }

    /// What kind of file the entry is, without following a link.
    pub async fn file_type(&self) -> io::Result<FileType> {
        self.file_type
            .clone()
            .map_err(|(kind, message)| io::Error::new(kind, message))
    }

    /// The entry's metadata, without following a link.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        let entry = std::sync::Arc::clone(&self.inner);
        blocking(move || entry.metadata()).await
    }
}
