//! Socket options: one get and one set over each platform's call, the
//! option names the socket builders and the streams use, and the linger
//! value in both platforms' forms.

use std::io;
use std::time::Duration;

use crate::io::raw::Raw;

/// A plain value an option holds: every bit pattern of its size is one,
/// so the kernel may write any into it.
pub(crate) trait Plain: Copy {}

#[cfg(unix)]
mod imp {
    use super::Plain;

    pub(crate) type Int = libc::c_int;
    pub(crate) type Linger = libc::linger;

    impl Plain for Int {}
    impl Plain for Linger {}

    pub(crate) const SOL_SOCKET: Int = libc::SOL_SOCKET;
    pub(crate) const SO_KEEPALIVE: Int = libc::SO_KEEPALIVE;
    pub(crate) const SO_REUSEADDR: Int = libc::SO_REUSEADDR;
    #[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
    pub(crate) const SO_REUSEPORT: Int = libc::SO_REUSEPORT;
    pub(crate) const SO_SNDBUF: Int = libc::SO_SNDBUF;
    pub(crate) const SO_RCVBUF: Int = libc::SO_RCVBUF;
    pub(crate) const SO_LINGER: Int = libc::SO_LINGER;
    pub(crate) const IPPROTO_IP: Int = libc::IPPROTO_IP;
    pub(crate) const IP_TOS: Int = libc::IP_TOS;
    pub(crate) const IPPROTO_IPV6: Int = libc::IPPROTO_IPV6;
    pub(crate) const IPV6_TCLASS: Int = libc::IPV6_TCLASS;
    pub(crate) const IPV6_V6ONLY: Int = libc::IPV6_V6ONLY;
    pub(crate) const IPPROTO_TCP: Int = libc::IPPROTO_TCP;

    pub(crate) fn linger(on: bool, secs: u64) -> Linger {
        libc::linger {
            l_onoff: Int::from(on),
            l_linger: Int::try_from(secs).unwrap_or(Int::MAX),
        }
    }

    pub(crate) fn linger_of(linger: Linger) -> Option<u64> {
        (linger.l_onoff != 0).then(|| u64::try_from(linger.l_linger).unwrap_or(0))
    }

    /// `getsockopt(2)` into `value`; returns the length the kernel wrote.
    pub(crate) unsafe fn get_raw(
        raw: super::Raw,
        level: Int,
        name: Int,
        value: *mut u8,
        len: usize,
    ) -> std::io::Result<usize> {
        let mut len = len as libc::socklen_t;
        // SAFETY: the caller's `value` holds `len` bytes.
        let r = unsafe { libc::getsockopt(raw, level, name, value.cast(), &mut len) };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(len as usize)
    }

    /// `setsockopt(2)` from `value`.
    pub(crate) unsafe fn set_raw(
        raw: super::Raw,
        level: Int,
        name: Int,
        value: *const u8,
        len: usize,
    ) -> std::io::Result<()> {
        // SAFETY: the caller's `value` holds `len` bytes.
        let r = unsafe { libc::setsockopt(raw, level, name, value.cast(), len as libc::socklen_t) };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::Plain;
    use crate::syscore::windows as win;

    pub(crate) type Int = i32;
    pub(crate) type Linger = win::LINGER;

    impl Plain for Int {}
    impl Plain for Linger {}

    pub(crate) use win::{
        IP_TOS, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPV6_V6ONLY, SO_KEEPALIVE, SO_LINGER,
        SO_RCVBUF, SO_REUSEADDR, SO_SNDBUF, SOL_SOCKET,
    };

    pub(crate) fn linger(on: bool, secs: u64) -> Linger {
        win::LINGER {
            l_onoff: u16::from(on),
            l_linger: u16::try_from(secs).unwrap_or(u16::MAX),
        }
    }

    pub(crate) fn linger_of(linger: Linger) -> Option<u64> {
        (linger.l_onoff != 0).then_some(u64::from(linger.l_linger))
    }

    /// `getsockopt` into `value`; returns the length Winsock wrote.
    pub(crate) unsafe fn get_raw(
        raw: super::Raw,
        level: Int,
        name: Int,
        value: *mut u8,
        len: usize,
    ) -> std::io::Result<usize> {
        let mut len = len as i32;
        // SAFETY: the caller's `value` holds `len` bytes.
        if unsafe { win::getsockopt(raw as win::SOCKET, level, name, value, &mut len) } != 0 {
            return Err(win::wsa_error());
        }
        Ok(len as usize)
    }

    /// `setsockopt` from `value`.
    pub(crate) unsafe fn set_raw(
        raw: super::Raw,
        level: Int,
        name: Int,
        value: *const u8,
        len: usize,
    ) -> std::io::Result<()> {
        // SAFETY: the caller's `value` holds `len` bytes.
        if unsafe { win::setsockopt(raw as win::SOCKET, level, name, value, len as i32) } != 0 {
            return Err(win::wsa_error());
        }
        Ok(())
    }
}

pub(crate) use imp::*;

/// The option `name` at `level` of the socket `raw`.
pub(crate) fn get<T: Plain>(raw: Raw, level: Int, name: Int) -> io::Result<T> {
    let mut value = core::mem::MaybeUninit::<T>::zeroed();
    // SAFETY: `value` holds a `T`; zeroed, and `T: Plain`, it is one
    // whatever the kernel writes into it.
    unsafe {
        get_raw(raw, level, name, value.as_mut_ptr().cast(), size_of::<T>())?;
        Ok(value.assume_init())
    }
}

/// Sets the option `name` at `level` of the socket `raw` to `value`.
pub(crate) fn set<T: Plain>(raw: Raw, level: Int, name: Int, value: T) -> io::Result<()> {
    // SAFETY: `value` is a `T` of our own.
    unsafe {
        set_raw(
            raw,
            level,
            name,
            core::ptr::from_ref(&value).cast(),
            size_of::<T>(),
        )
    }
}

/// A flag option.
pub(crate) fn flag(raw: Raw, level: Int, name: Int) -> io::Result<bool> {
    get::<Int>(raw, level, name).map(|v| v != 0)
}

/// Sets a flag option.
pub(crate) fn set_flag(raw: Raw, level: Int, name: Int, on: bool) -> io::Result<()> {
    set(raw, level, name, Int::from(on))
}

/// A size option, in bytes.
pub(crate) fn size(raw: Raw, level: Int, name: Int) -> io::Result<u32> {
    get::<Int>(raw, level, name).map(|v| u32::try_from(v).unwrap_or(0))
}

/// Sets a size option, in bytes.
pub(crate) fn set_size(raw: Raw, level: Int, name: Int, bytes: u32) -> io::Result<()> {
    set(raw, level, name, Int::try_from(bytes).unwrap_or(Int::MAX))
}

/// A count option.
pub(crate) fn count(raw: Raw, level: Int, name: Int) -> io::Result<u32> {
    size(raw, level, name)
}

/// Sets a count option; a count past what the option holds is the caller's
/// mistake, named `what`.
pub(crate) fn set_count(raw: Raw, level: Int, name: Int, n: u32, what: &str) -> io::Result<()> {
    set(raw, level, name, int(u64::from(n), what)?)
}

/// `n` as the option's integer, or `InvalidInput` naming `what`.
pub(crate) fn int(n: u64, what: &str) -> io::Result<Int> {
    Int::try_from(n).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the {what} is more than this system takes"),
        )
    })
}

/// `time` in whole units of `unit`, a part of one counting as one more.
pub(crate) fn whole(time: Duration, unit: Duration) -> u64 {
    let units = time.as_nanos().div_ceil(unit.as_nanos());
    u64::try_from(units).unwrap_or(u64::MAX)
}

/// The error of an option the system does not have, named `name`.
pub(crate) fn unsupported(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("this system has no {name} option"),
    )
}

/// `SO_LINGER`: how long a close waits for unsent data, `None` if it does
/// not wait.
pub(crate) fn linger_time(raw: Raw) -> io::Result<Option<Duration>> {
    get::<Linger>(raw, SOL_SOCKET, SO_LINGER).map(|l| linger_of(l).map(Duration::from_secs))
}

/// Sets `SO_LINGER`: `None` closes at once, sending what is left in the
/// background; `Some(time)` waits up to `time` (whole seconds) for it.
pub(crate) fn set_linger_time(raw: Raw, time: Option<Duration>) -> io::Result<()> {
    let value = match time {
        Some(time) => linger(true, time.as_secs()),
        None => linger(false, 0),
    };
    set(raw, SOL_SOCKET, SO_LINGER, value)
}
