//! Linux: the hand-rolled io_uring ring and the probe that says whether the
//! kernel lets this process use it.

#[cfg(test)]
mod bench;

#[cfg(feature = "iouring")]
pub(crate) mod iouring;

#[cfg(feature = "iouring")]
pub(crate) mod probe;
