//! A Unix socket address in its raw form, made from a path or a name for a
//! call, and std's address made from the raw form the kernel answers with.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net as unix;
use std::path::Path;

/// A Unix socket address in its raw form: a path, or on Linux an
/// abstract name. Made once on the caller's stack and copied by value, so
/// no send, connect or bind allocates for its address.
#[derive(Clone, Copy)]
pub(crate) struct UnixAddr {
    pub(crate) raw: libc::sockaddr_un,
    pub(crate) len: libc::socklen_t,
}

impl UnixAddr {
    /// The address of the socket file at `path`.
    pub(crate) fn path(path: &Path) -> io::Result<UnixAddr> {
        let bytes = path.as_os_str().as_bytes();
        if has_nul(bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the socket path holds a NUL byte",
            ));
        }
        // The path, then its terminating zero.
        Self::from_bytes(bytes, 1)
    }

    /// The address in Linux's abstract namespace named `name`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn abstract_name(name: &[u8]) -> io::Result<UnixAddr> {
        let mut with_nul = [0u8; 108];
        let len = name.len() + 1;
        if len > with_nul.len() {
            return Err(too_long());
        }
        with_nul[1..len].copy_from_slice(name);
        Self::from_bytes(&with_nul[..len], 0)
    }

    /// The raw form of std's address: a path, or (Linux) an abstract name.
    pub(crate) fn of(addr: &unix::SocketAddr) -> io::Result<UnixAddr> {
        if let Some(path) = addr.as_pathname() {
            return Self::path(path);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            #[cfg(target_os = "android")]
            use std::os::android::net::SocketAddrExt;
            #[cfg(target_os = "linux")]
            use std::os::linux::net::SocketAddrExt;
            if let Some(name) = addr.as_abstract_name() {
                return Self::abstract_name(name);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an unnamed Unix socket address names no socket",
        ))
    }

    /// `bytes` in `sun_path`, followed by `extra` counted zero bytes.
    fn from_bytes(bytes: &[u8], extra: usize) -> io::Result<UnixAddr> {
        // SAFETY: plain data; all zeroes is a valid value.
        let mut raw: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        if bytes.len() + extra > raw.sun_path.len() {
            return Err(too_long());
        }
        raw.sun_family = libc::AF_UNIX as libc::sa_family_t;
        // SAFETY: `c_char` and `u8` have one layout, and `bytes` fits `sun_path` (checked above).
        let path = unsafe {
            core::slice::from_raw_parts_mut(raw.sun_path.as_mut_ptr().cast::<u8>(), bytes.len())
        };
        path.copy_from_slice(bytes);
        let len = core::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + extra;
        Ok(UnixAddr {
            raw,
            len: len as libc::socklen_t,
        })
    }

    pub(crate) fn as_ptr(&self) -> *const libc::sockaddr {
        (&self.raw as *const libc::sockaddr_un).cast()
    }
}

/// Whether `bytes` holds a zero byte, by the C library's own scan.
fn has_nul(bytes: &[u8]) -> bool {
    // An empty slice's pointer may dangle, and C wants a valid pointer even for no bytes.
    if bytes.is_empty() {
        return false;
    }
    // SAFETY: reads at most `bytes.len()` bytes of `bytes`, which lie in one allocation.
    !unsafe { libc::memchr(bytes.as_ptr().cast(), 0, bytes.len()) }.is_null()
}

fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "the socket address is too long",
    )
}

impl core::fmt::Debug for UnixAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match unix_addr(&self.raw, self.len) {
            Ok(addr) => write!(f, "{addr:?}"),
            Err(_) => f.write_str("(unnamed)"),
        }
    }
}

/// A Unix socket address from its raw form: a path, an abstract name
/// (Linux), or unnamed.
pub(crate) fn unix_addr(
    storage: &libc::sockaddr_un,
    len: libc::socklen_t,
) -> io::Result<unix::SocketAddr> {
    let base = core::mem::offset_of!(libc::sockaddr_un, sun_path);
    let path_len = (len as usize)
        .saturating_sub(base)
        .min(storage.sun_path.len());
    // SAFETY: `c_char` and `u8` have one layout; the slice is within
    // `sun_path`.
    let raw: &[u8] =
        unsafe { core::slice::from_raw_parts(storage.sun_path.as_ptr().cast(), path_len) };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let [0, name @ ..] = raw
        && !name.is_empty()
    {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        return unix::SocketAddr::from_abstract_name(name);
    }
    let path = raw.split(|&b| b == 0).next().unwrap_or_default();
    if path.is_empty() {
        return unnamed();
    }
    unix::SocketAddr::from_pathname(std::ffi::OsStr::from_bytes(path))
}

/// The unnamed address, from an empty path: no socket is made for it, so a
/// datagram the kernel handed over is never lost to a process with no
/// descriptor to spare.
fn unnamed() -> io::Result<unix::SocketAddr> {
    unix::SocketAddr::from_pathname("")
}

#[cfg(test)]
mod tests;
