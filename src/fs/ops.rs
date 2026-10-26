//! The file system's calls by path: reading and writing whole files, metadata,
//! directories, links, renames and copies, and [`OpenOptions`] and
//! [`DirBuilder`]. Each runs on the blocking pool, as a regular file's calls
//! do; the task waits for its end without holding its shard.

use std::fs::{Metadata, Permissions};
use std::io;
use std::path::{Path, PathBuf};

use crate::io::Handle;

/// Runs `call` on the blocking pool and answers its result; a call whose task
/// was lost (its runtime shut down) is an error, not a panic.
pub(crate) async fn blocking<T, F>(call: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    match crate::runtime::spawn_blocking(call).join().await {
        Ok(result) => result.inspect_err(crate::rt::counters::io_error),
        Err(lost) => Err(lost.into()),
    }
}

/// The whole file at `path`.
pub async fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::read(path)).await
}

/// The whole file at `path`, which must be UTF-8.
pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::read_to_string(path)).await
}

/// Writes `contents` as the whole file at `path`, creating it or cutting it
/// down first.
pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let (path, contents) = (path.as_ref().to_path_buf(), contents.as_ref().to_vec());
    blocking(move || std::fs::write(path, contents)).await
}

/// The metadata of what `path` names, through links.
pub async fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::metadata(path)).await
}

/// The metadata of what `path` names, a link itself rather than its target.
pub async fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::symlink_metadata(path)).await
}

/// Whether `path` names something: `Ok(false)` when it does not, an error
/// when that cannot be told (no permission, for one).
pub async fn try_exists(path: impl AsRef<Path>) -> io::Result<bool> {
    let path = path.as_ref().to_path_buf();
    blocking(move || path.try_exists()).await
}

/// Creates the directory `path`; its parent must exist.
pub async fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::create_dir(path)).await
}

/// Creates the directory `path` and every missing parent.
pub async fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::create_dir_all(path)).await
}

/// Removes the file at `path`.
pub async fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::remove_file(path)).await
}

/// Removes the empty directory at `path`.
pub async fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::remove_dir(path)).await
}

/// Removes the directory at `path` and everything in it.
pub async fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::remove_dir_all(path)).await
}

/// Renames `from` to `to`, replacing `to` if it is a file.
pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref().to_path_buf(), to.as_ref().to_path_buf());
    blocking(move || std::fs::rename(from, to)).await
}

/// Copies the file `from` to `to`, with its permissions; answers the bytes
/// copied.
pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref().to_path_buf(), to.as_ref().to_path_buf());
    blocking(move || std::fs::copy(from, to)).await
}

/// Makes `link` a new name of the file `original` (a hard link).
pub async fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    blocking(move || std::fs::hard_link(original, link)).await
}

/// Makes `link` a symbolic link to `original`.
#[cfg(unix)]
pub async fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    blocking(move || std::os::unix::fs::symlink(original, link)).await
}

/// Makes `link` a symbolic link to the file `original`.
#[cfg(windows)]
pub async fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    blocking(move || std::os::windows::fs::symlink_file(original, link)).await
}

/// Makes `link` a symbolic link to the directory `original`.
#[cfg(windows)]
pub async fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    blocking(move || std::os::windows::fs::symlink_dir(original, link)).await
}

/// Where the symbolic link `path` points.
pub async fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::read_link(path)).await
}

/// The absolute path of `path`, every link and `..` resolved.
pub async fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::canonicalize(path)).await
}

/// Sets the permissions of what `path` names.
pub async fn set_permissions(path: impl AsRef<Path>, perm: Permissions) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::set_permissions(path, perm)).await
}

/// How to open a file: the options of std's `OpenOptions`, opened on the
/// blocking pool into a [`Handle`].
#[derive(Clone, Debug)]
pub struct OpenOptions(std::fs::OpenOptions);

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    /// No option set: neither read nor write.
    pub fn new() -> Self {
        Self(std::fs::OpenOptions::new())
    }

    /// Opens for reading.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.0.read(read);
        self
    }

    /// Opens for writing.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.0.write(write);
        self
    }

    /// Opens for writing at the end, whatever the cursor.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.0.append(append);
        self
    }

    /// Cuts an existing file down to nothing when it opens.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.0.truncate(truncate);
        self
    }

    /// Creates the file if it does not exist.
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.0.create(create);
        self
    }

    /// Creates the file, failing if it exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.0.create_new(create_new);
        self
    }

    /// The mode bits a created file gets (before the umask).
    #[cfg(unix)]
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(&mut self.0, mode);
        self
    }

    /// Flags passed to `open` beside the ones the options set.
    #[cfg(unix)]
    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut self.0, flags);
        self
    }

    /// The access mode `CreateFileW` gets, over read and write.
    #[cfg(windows)]
    pub fn access_mode(&mut self, access: u32) -> &mut Self {
        std::os::windows::fs::OpenOptionsExt::access_mode(&mut self.0, access);
        self
    }

    /// The share mode `CreateFileW` gets.
    #[cfg(windows)]
    pub fn share_mode(&mut self, share: u32) -> &mut Self {
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut self.0, share);
        self
    }

    /// Flags `CreateFileW` gets beside the ones the options set.
    #[cfg(windows)]
    pub fn custom_flags(&mut self, flags: u32) -> &mut Self {
        std::os::windows::fs::OpenOptionsExt::custom_flags(&mut self.0, flags);
        self
    }

    /// The attributes a created file gets.
    #[cfg(windows)]
    pub fn attributes(&mut self, attributes: u32) -> &mut Self {
        std::os::windows::fs::OpenOptionsExt::attributes(&mut self.0, attributes);
        self
    }

    /// The security quality of service flags `CreateFileW` gets.
    #[cfg(windows)]
    pub fn security_qos_flags(&mut self, flags: u32) -> &mut Self {
        std::os::windows::fs::OpenOptionsExt::security_qos_flags(&mut self.0, flags);
        self
    }

    /// Opens the file at `path` with these options.
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<Handle<std::fs::File>> {
        let (options, path) = (self.0.clone(), path.as_ref().to_path_buf());
        let file = blocking(move || options.open(path)).await?;
        Handle::new(file)
    }
}

impl From<std::fs::OpenOptions> for OpenOptions {
    fn from(options: std::fs::OpenOptions) -> Self {
        Self(options)
    }
}

/// How to create a directory: recursively or not, and with which mode.
#[derive(Debug, Default)]
pub struct DirBuilder {
    recursive: bool,
    #[cfg(unix)]
    mode: Option<u32>,
}

impl DirBuilder {
    /// Creates one directory, with the default mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates every missing parent too.
    pub fn recursive(&mut self, recursive: bool) -> &mut Self {
        self.recursive = recursive;
        self
    }

    /// The mode bits the directories get (before the umask).
    #[cfg(unix)]
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode);
        self
    }

    /// Creates the directory `path`.
    pub async fn create(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(self.recursive);
        #[cfg(unix)]
        if let Some(mode) = self.mode {
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, mode);
        }
        let path = path.as_ref().to_path_buf();
        blocking(move || builder.create(path)).await
    }
}
