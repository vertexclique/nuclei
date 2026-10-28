//! The error of a timeout.

use std::fmt;
use std::io;

/// A [`timeout`](super::timeout()) ran out before its future completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Elapsed(());

impl Elapsed {
    pub(super) const fn new() -> Self {
        Self(())
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

impl From<Elapsed> for io::Error {
    fn from(_: Elapsed) -> Self {
        io::ErrorKind::TimedOut.into()
    }
}
