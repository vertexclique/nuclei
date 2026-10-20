#[cfg(feature = "epoll")]
mod epoll;
#[cfg(feature = "epoll")]
pub(crate) use epoll::*;

#[cfg(feature = "iouring")]
mod iouring;
#[cfg(feature = "iouring")]
pub(crate) use iouring::*;

#[cfg(test)]
mod phase0;

#[cfg(all(target_os = "linux", feature = "iouring"))]
pub(crate) mod probe;
